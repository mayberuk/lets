pub(super) const QUERY: &str = r"
(function_declaration name: (identifier) @name) @def
(class_declaration name: (identifier) @name) @def
(method_definition name: (_) @name) @def

(class_declaration name: (identifier) @scope.name) @scope
";
