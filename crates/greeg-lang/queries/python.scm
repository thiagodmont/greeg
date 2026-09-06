; greeg tags for Python (tree-sitter-python 0.25)
(class_definition name: (identifier) @name superclasses: (argument_list)? @supers) @def.class
(function_definition name: (identifier) @name) @def.function
(module (expression_statement (assignment left: (identifier) @name) @def.variable))
(module (type_alias_statement left: (type (identifier) @name)) @def.typealias)
(class_definition body: (block (expression_statement (assignment left: (identifier) @name) @def.field)))
; instance and class attributes assigned in methods (`self.x = …`, `cls.x = …`, at any
; depth inside the method); `finish` moves them to the class and keeps one per name
((assignment left: (attribute object: (identifier) @recv attribute: (identifier) @name) @def.field)
 (#any-of? @recv "self" "cls"))
; module-level names under `if`/`elif`/`else`/`try`/`except` one level deep
; (`if TYPE_CHECKING:`, `if __name__ == "__main__":`, optional imports)
(module (if_statement consequence: (block (expression_statement (assignment left: (identifier) @name) @def.variable))))
(module (if_statement alternative: (elif_clause consequence: (block (expression_statement (assignment left: (identifier) @name) @def.variable)))))
(module (if_statement alternative: (else_clause body: (block (expression_statement (assignment left: (identifier) @name) @def.variable)))))
(module (try_statement body: (block (expression_statement (assignment left: (identifier) @name) @def.variable))))
(module (try_statement (except_clause (block (expression_statement (assignment left: (identifier) @name) @def.variable)))))
(module (try_statement (else_clause body: (block (expression_statement (assignment left: (identifier) @name) @def.variable)))))
(import_statement) @import
(import_from_statement) @import
(comment) @noncode.comment
(string) @noncode.string
(function_definition body: (block . (expression_statement (string) @noncode.docstring)))
(class_definition body: (block . (expression_statement (string) @noncode.docstring)))
(module . (expression_statement (string) @noncode.docstring))
