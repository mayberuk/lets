/// `impl<T> Store<T>` nests its name in a `generic_type`; the segment is `Store`, not `Store<T>`.
pub(super) const QUERY: &str = r"
(function_item name: (identifier) @name) @def
(struct_item name: (type_identifier) @name) @def
(enum_item name: (type_identifier) @name) @def
(trait_item name: (type_identifier) @name) @def

(impl_item
  type: [
    (type_identifier) @scope.name
    (generic_type type: (type_identifier) @scope.name)
  ]) @scope
";
