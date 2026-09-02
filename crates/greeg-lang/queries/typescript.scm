; greeg tags for TypeScript / TSX (tree-sitter-typescript 0.23)
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(function_signature name: (identifier) @name) @def.function
(class_declaration name: (_) @name (class_heritage)? @supers) @def.class
(abstract_class_declaration name: (_) @name (class_heritage)? @supers) @def.class
(class name: (_) @name (class_heritage)? @supers) @def.class
(method_definition name: (_) @name) @def.method
(method_signature name: (_) @name) @def.method
(abstract_method_signature name: (_) @name) @def.method
(public_field_definition name: (_) @name) @def.field
(property_signature name: (_) @name) @def.field
(interface_declaration name: (type_identifier) @name (extends_type_clause)? @supers) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.typealias
(enum_declaration name: (identifier) @name) @def.enum
(enum_body (property_identifier) @name @def.variant)
(enum_assignment name: (property_identifier) @name) @def.variant
(internal_module name: (_) @name) @def.module
(module name: (_) @name) @def.module
(program (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @def.function))
(program (variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @def.function))
(export_statement declaration: (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @def.function))
(export_statement declaration: (variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @def.function))
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))
(export_statement declaration: (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(export_statement declaration: (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))
(assignment_expression left: (member_expression property: (property_identifier) @name) right: [(arrow_function) (function_expression)]) @def.function
(pair key: (property_identifier) @name value: [(arrow_function) (function_expression)]) @def.function
(import_statement) @import
(export_statement source: (string)) @import
((call_expression function: (identifier) @_req arguments: (arguments (string))) @import (#eq? @_req "require"))
(comment) @noncode.comment
(string) @noncode.string
(template_string) @noncode.string
(regex) @noncode.string
