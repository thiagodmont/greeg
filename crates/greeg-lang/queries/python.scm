; greeg tags for Python (tree-sitter-python 0.25)
(class_definition name: (identifier) @name superclasses: (argument_list)? @supers) @def.class
(function_definition name: (identifier) @name) @def.function
(module (expression_statement (assignment left: (identifier) @name) @def.variable))
(module (type_alias_statement left: (type (identifier) @name)) @def.typealias)
(class_definition body: (block (expression_statement (assignment left: (identifier) @name) @def.field)))
(import_statement) @import
(import_from_statement) @import
(comment) @noncode.comment
(string) @noncode.string
(function_definition body: (block . (expression_statement (string) @noncode.docstring)))
(class_definition body: (block . (expression_statement (string) @noncode.docstring)))
(module . (expression_statement (string) @noncode.docstring))
