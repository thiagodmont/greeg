; greeg tags for Rust (tree-sitter-rust 0.24)
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(union_item name: (type_identifier) @name) @def.struct
(trait_item name: (type_identifier) @name bounds: (trait_bounds)? @supers) @def.trait
(type_item name: (type_identifier) @name) @def.typealias
(associated_type name: (type_identifier) @name) @def.typealias
(mod_item name: (identifier) @name) @def.module
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant
(macro_definition name: (identifier) @name) @def.macro
(impl_item trait: (_)? @supers type: (_) @name) @def.impl
(field_declaration name: (field_identifier) @name) @def.field
(enum_variant name: (identifier) @name) @def.variant
(macro_invocation (token_tree) @macro_body)
(use_declaration) @import
(extern_crate_declaration) @import
((mod_item !body) @import)
(line_comment) @noncode.comment
(block_comment) @noncode.comment
(string_literal) @noncode.string
(raw_string_literal) @noncode.string
(char_literal) @noncode.string
