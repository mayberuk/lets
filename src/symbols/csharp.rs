/// No constructors: each shares its type's name and would make `#Store` ambiguous.
pub(super) const QUERY: &str = r"
(method_declaration name: (identifier) @name) @def
(class_declaration name: (identifier) @name) @def
(struct_declaration name: (identifier) @name) @def
(interface_declaration name: (identifier) @name) @def
(record_declaration name: (identifier) @name) @def
(enum_declaration name: (identifier) @name) @def

(class_declaration name: (identifier) @scope.name) @scope
(struct_declaration name: (identifier) @scope.name) @scope
(interface_declaration name: (identifier) @scope.name) @scope
(record_declaration name: (identifier) @scope.name) @scope
(namespace_declaration name: (identifier) @scope.name) @scope
";
