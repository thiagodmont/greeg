; greeg tags for Kotlin (tree-sitter-kotlin-sg 0.4)
(class_declaration (type_identifier) @name (delegation_specifier)* @supers) @def.class
(object_declaration (type_identifier) @name (delegation_specifier)* @supers) @def.object
(companion_object) @def.object
(function_declaration (simple_identifier) @name) @def.function
(property_declaration (variable_declaration (simple_identifier) @name)) @def.variable
(property_declaration (multi_variable_declaration (variable_declaration (simple_identifier) @name))) @def.variable
(class_parameter (binding_pattern_kind) (simple_identifier) @name) @def.field
(type_alias (type_identifier) @name) @def.typealias
(enum_entry (simple_identifier) @name) @def.variant
(secondary_constructor) @def.method
(import_header) @import
(package_header) @package
(line_comment) @noncode.comment
(multiline_comment) @noncode.comment
(string_literal) @noncode.string
(character_literal) @noncode.string
