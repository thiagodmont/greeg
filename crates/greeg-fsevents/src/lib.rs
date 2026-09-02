//! macOS FSEvents: read the kernel's persistent event log since a stored
//! event id to learn which directories changed (DESIGN.md §4.5 mode 2).
//!
//! CoreFoundation and CoreServices are loaded with `dlopen` on first use
//! rather than linked: loading the two frameworks at process start cost
//! ≈ 1 ms on every invocation (measured in M4), and only trees above 8,000
//! files ever ask for FSEvents.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString, c_void};
    use std::os::raw::{c_char, c_int};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    type CFRef = *const c_void;
    type CFIndex = isize;
    type StreamRef = *mut c_void;
    type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[repr(C)]
    struct Context {
        version: CFIndex,
        info: *mut c_void,
        retain: *const c_void,
        release: *const c_void,
        copy_description: *const c_void,
    }

    const FLAG_MUST_SCAN_SUBDIRS: u32 = 0x01;
    const FLAG_KERNEL_DROPPED: u32 = 0x04;
    const FLAG_USER_DROPPED: u32 = 0x08;
    const FLAG_HISTORY_DONE: u32 = 0x10;
    const FLAG_IDS_WRAPPED: u32 = 0x20;
    const FLAG_ROOT_CHANGED: u32 = 0x40;
    const CREATE_NO_DEFER: u32 = 0x02;

    #[allow(non_snake_case)]
    struct Api {
        CFStringCreateWithFileSystemRepresentation: unsafe extern "C" fn(CFRef, *const c_char) -> CFRef,
        CFArrayCreateMutable: unsafe extern "C" fn(CFRef, CFIndex, *const c_void) -> CFRef,
        CFArrayAppendValue: unsafe extern "C" fn(CFRef, CFRef),
        CFRunLoopGetCurrent: unsafe extern "C" fn() -> CFRef,
        CFRunLoopRunInMode: unsafe extern "C" fn(CFRef, f64, u8) -> i32,
        CFRelease: unsafe extern "C" fn(CFRef),
        kCFTypeArrayCallBacks: *const c_void,
        kCFRunLoopDefaultMode: CFRef,
        FSEventsGetCurrentEventId: unsafe extern "C" fn() -> u64,
        FSEventStreamCreate: unsafe extern "C" fn(CFRef, Callback, *const Context, CFRef, u64, f64, u32) -> StreamRef,
        FSEventStreamScheduleWithRunLoop: unsafe extern "C" fn(StreamRef, CFRef, CFRef),
        FSEventStreamStart: unsafe extern "C" fn(StreamRef) -> u8,
        FSEventStreamStop: unsafe extern "C" fn(StreamRef),
        FSEventStreamInvalidate: unsafe extern "C" fn(StreamRef),
        FSEventStreamRelease: unsafe extern "C" fn(StreamRef),
    }

    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    unsafe fn sym(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
        if p.is_null() { None } else { Some(p) }
    }

    #[allow(clippy::missing_transmute_annotations)]
    fn load() -> Option<Api> {
        unsafe {
            let cf = libc::dlopen(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
            let cs = libc::dlopen(c"/System/Library/Frameworks/CoreServices.framework/CoreServices".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
            if cf.is_null() || cs.is_null() {
                return None;
            }
            macro_rules! f {
                ($h:expr, $name:literal) => {
                    std::mem::transmute(sym($h, $name)?)
                };
            }
            // data symbols: the address holds the value
            let callbacks = sym(cf, "kCFTypeArrayCallBacks")?;
            let mode = *(sym(cf, "kCFRunLoopDefaultMode")? as *const CFRef);
            Some(Api {
                CFStringCreateWithFileSystemRepresentation: f!(cf, "CFStringCreateWithFileSystemRepresentation"),
                CFArrayCreateMutable: f!(cf, "CFArrayCreateMutable"),
                CFArrayAppendValue: f!(cf, "CFArrayAppendValue"),
                CFRunLoopGetCurrent: f!(cf, "CFRunLoopGetCurrent"),
                CFRunLoopRunInMode: f!(cf, "CFRunLoopRunInMode"),
                CFRelease: f!(cf, "CFRelease"),
                kCFTypeArrayCallBacks: callbacks as *const c_void,
                kCFRunLoopDefaultMode: mode,
                FSEventsGetCurrentEventId: f!(cs, "FSEventsGetCurrentEventId"),
                FSEventStreamCreate: f!(cs, "FSEventStreamCreate"),
                FSEventStreamScheduleWithRunLoop: f!(cs, "FSEventStreamScheduleWithRunLoop"),
                FSEventStreamStart: f!(cs, "FSEventStreamStart"),
                FSEventStreamStop: f!(cs, "FSEventStreamStop"),
                FSEventStreamInvalidate: f!(cs, "FSEventStreamInvalidate"),
                FSEventStreamRelease: f!(cs, "FSEventStreamRelease"),
            })
        }
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(load).as_ref()
    }

    struct State {
        dirs: Vec<String>,
        done: bool,
        unreliable: bool,
    }

    extern "C" fn cb(_s: StreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _ids: *const u64) {
        let st = unsafe { &mut *(info as *mut State) };
        let paths = paths as *const *const c_char;
        for i in 0..n {
            let fl = unsafe { *flags.add(i) };
            if fl & FLAG_HISTORY_DONE != 0 {
                st.done = true;
                continue;
            }
            if fl & (FLAG_KERNEL_DROPPED | FLAG_USER_DROPPED | FLAG_IDS_WRAPPED | FLAG_MUST_SCAN_SUBDIRS | FLAG_ROOT_CHANGED) != 0 {
                st.unreliable = true;
            }
            let p = unsafe { CStr::from_ptr(*paths.add(i)) }.to_string_lossy().into_owned();
            st.dirs.push(p);
        }
    }

    /// Current FSEvents id, or 0 when the frameworks cannot be loaded.
    pub fn current_id() -> u64 {
        match api() {
            Some(a) => unsafe { (a.FSEventsGetCurrentEventId)() },
            None => 0,
        }
    }

    /// Directories (absolute, trailing slash) with events since `id`.
    /// `None` when the log could not be read reliably within `cutoff`.
    pub fn changed_dirs_since(id: u64, root: &str, cutoff: Duration) -> Option<Vec<String>> {
        let a = api()?;
        let mut st = State { dirs: Vec::new(), done: false, unreliable: false };
        let croot = CString::new(root).ok()?;
        unsafe {
            let cfpath = (a.CFStringCreateWithFileSystemRepresentation)(std::ptr::null(), croot.as_ptr());
            if cfpath.is_null() {
                return None;
            }
            let arr = (a.CFArrayCreateMutable)(std::ptr::null(), 0, a.kCFTypeArrayCallBacks);
            (a.CFArrayAppendValue)(arr, cfpath);
            let ctx = Context { version: 0, info: &mut st as *mut State as *mut c_void, retain: std::ptr::null(), release: std::ptr::null(), copy_description: std::ptr::null() };
            let stream = (a.FSEventStreamCreate)(std::ptr::null(), cb, &ctx, arr, id, 0.0, CREATE_NO_DEFER);
            if stream.is_null() {
                (a.CFRelease)(arr);
                (a.CFRelease)(cfpath);
                return None;
            }
            (a.FSEventStreamScheduleWithRunLoop)(stream, (a.CFRunLoopGetCurrent)(), a.kCFRunLoopDefaultMode);
            (a.FSEventStreamStart)(stream);
            let deadline = Instant::now() + cutoff;
            while !st.done && Instant::now() < deadline {
                (a.CFRunLoopRunInMode)(a.kCFRunLoopDefaultMode, 0.002, 1);
            }
            (a.FSEventStreamStop)(stream);
            (a.FSEventStreamInvalidate)(stream);
            (a.FSEventStreamRelease)(stream);
            (a.CFRelease)(arr);
            (a.CFRelease)(cfpath);
        }
        let _: c_int = 0;
        if !st.done || st.unreliable {
            return None;
        }
        st.dirs.sort();
        st.dirs.dedup();
        Some(st.dirs)
    }
}

#[cfg(target_os = "macos")]
pub use imp::{changed_dirs_since, current_id};

#[cfg(not(target_os = "macos"))]
pub fn current_id() -> u64 {
    0
}
#[cfg(not(target_os = "macos"))]
pub fn changed_dirs_since(_id: u64, _root: &str, _cutoff: std::time::Duration) -> Option<Vec<String>> {
    None
}
