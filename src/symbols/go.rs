/// A receiver type is not the method's ancestor node, so `@self.scope` names it within the match.
pub(super) const QUERY: &str = r"
(function_declaration name: (identifier) @name) @def

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (type_identifier) @self.scope
        (pointer_type (type_identifier) @self.scope)
        (generic_type type: (type_identifier) @self.scope)
      ]))
  name: (field_identifier) @name) @def

(type_declaration (type_spec name: (type_identifier) @name)) @def
";
