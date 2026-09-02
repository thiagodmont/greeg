#!/bin/sh
# Timing vs ripgrep at 1/4/all threads. Usage: timing.sh CORPORA_DIR GREEG_BIN
C=$1; B=$2
run() { d=$1; shift; echo "== $d: $*"; (cd $C/$d && hyperfine -N -w 2 -r 8 --export-json /dev/null "rg -n $* ." "rg -j1 -n $* ." "rg -j4 -n $* ." "$B --no-ladder $* ." "$B --no-ladder --budget 0 --mode files $* ." 2>&1 | grep -E "Benchmark|Time \(mean"); }
run tokio "'fn poll'"
run tokio "-w Waker"
run django get_queryset
run django "-w request"
run ktor respond
run ktor "'fun respond'"
run TypeScript createSourceFile
run TypeScript "-w node"
run rust "-w HirId"
