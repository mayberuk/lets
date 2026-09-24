/// An extension names its type as a `user_type`: it scopes methods without being a definition.
pub(super) const QUERY: &str = r"
(function_declaration name: (simple_identifier) @name) @def
(protocol_function_declaration name: (simple_identifier) @name) @def
(class_declaration name: (type_identifier) @name) @def
(protocol_declaration name: (type_identifier) @name) @def

(class_declaration
  name: [
    (type_identifier) @scope.name
    (user_type (type_identifier) @scope.name)
  ]) @scope
(protocol_declaration name: (type_identifier) @scope.name) @scope
";
