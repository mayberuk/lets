pub(super) const QUERY: &str = r"
(function_definition name: (name) @name) @def
(method_declaration name: (name) @name) @def
(class_declaration name: (name) @name) @def
(interface_declaration name: (name) @name) @def
(trait_declaration name: (name) @name) @def
(enum_declaration name: (name) @name) @def

(class_declaration name: (name) @scope.name) @scope
(interface_declaration name: (name) @scope.name) @scope
(trait_declaration name: (name) @scope.name) @scope
(enum_declaration name: (name) @scope.name) @scope
";
