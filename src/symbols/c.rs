/// `#not-eq?` keeps `typedef struct Store {…} Store;` from answering `#Store` twice on one line.
pub(super) const QUERY: &str = r"
(function_definition
  declarator: [
    (function_declarator declarator: (identifier) @name)
    (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
  ]) @def

(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @def
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @def
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @def

(type_definition
  type: [
    (struct_specifier name: (type_identifier) @tag)
    (union_specifier name: (type_identifier) @tag)
    (enum_specifier name: (type_identifier) @tag)
  ]
  declarator: (type_identifier) @name
  (#not-eq? @tag @name)) @def

(type_definition
  type: [
    (struct_specifier !name)
    (union_specifier !name)
    (enum_specifier !name)
    (primitive_type)
    (sized_type_specifier)
    (type_identifier)
  ]
  declarator: (type_identifier) @name) @def
";
