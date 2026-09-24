/// `class Outer::Inner` defines only `Inner`; the `Outer` before it is a reference.
pub(super) const QUERY: &str = r"
(method name: (_) @name) @def
(singleton_method name: (_) @name) @def

(class name: [(constant) @name (scope_resolution name: (constant) @name)]) @def
(module name: [(constant) @name (scope_resolution name: (constant) @name)]) @def

(class name: [(constant) @scope.name (scope_resolution name: (constant) @scope.name)]) @scope
(module name: [(constant) @scope.name (scope_resolution name: (constant) @scope.name)]) @scope
";
