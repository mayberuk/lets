/// Constructors are left out: they carry the class name, so every `#Store` would be ambiguous.
pub(super) const QUERY: &str = r"
(method_declaration name: (identifier) @name) @def
(class_declaration name: (identifier) @name) @def
(interface_declaration name: (identifier) @name) @def
(enum_declaration name: (identifier) @name) @def
(record_declaration name: (identifier) @name) @def

(class_declaration name: (identifier) @scope.name) @scope
(interface_declaration name: (identifier) @scope.name) @scope
(enum_declaration name: (identifier) @scope.name) @scope
(record_declaration name: (identifier) @scope.name) @scope
";
