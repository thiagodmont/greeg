; greeg tags for JavaScript (tree-sitter-javascript 0.25)
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(class_declaration name: (_) @name (class_heritage)? @supers) @def.class
(class name: (_) @name (class_heritage)? @supers) @def.class
(method_definition name: (_) @name) @def.method
(field_definition property: (_) @name) @def.field
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
