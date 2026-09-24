/// An out-of-line `Store::open()` names its class inside its own declarator, hence `@self.scope`.
pub(super) const QUERY: &str = r"
(function_definition
  declarator: [
    (function_declarator declarator: [
      (identifier) @name
      (field_identifier) @name
      (qualified_identifier
        scope: [
          (namespace_identifier) @self.scope
          (template_type name: (type_identifier) @self.scope)
        ]
        name: (identifier) @name)
    ])
    (pointer_declarator declarator: (function_declarator declarator: [
      (identifier) @name
      (field_identifier) @name
      (qualified_identifier
        scope: [
          (namespace_identifier) @self.scope
          (template_type name: (type_identifier) @self.scope)
        ]
        name: (identifier) @name)
    ]))
    (reference_declarator (function_declarator declarator: [
      (identifier) @name
      (field_identifier) @name
      (qualified_identifier
        scope: [
          (namespace_identifier) @self.scope
          (template_type name: (type_identifier) @self.scope)
        ]
        name: (identifier) @name)
    ]))
  ]) @def

(class_specifier name: (type_identifier) @name body: (field_declaration_list)) @def
(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @def
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @def
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @def

(class_specifier name: (type_identifier) @scope.name body: (field_declaration_list)) @scope
(struct_specifier name: (type_identifier) @scope.name body: (field_declaration_list)) @scope
(namespace_definition name: (namespace_identifier) @scope.name) @scope
";
