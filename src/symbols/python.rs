pub(super) const QUERY: &str = r"
(function_definition name: (identifier) @name) @def
(class_definition name: (identifier) @name) @def

(class_definition name: (identifier) @scope.name) @scope
";
