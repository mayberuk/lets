/// A pair's key hangs off the pair, so a table's only direct key child is its own name.
pub(super) const QUERY: &str = r"
(table [(bare_key) (dotted_key) (quoted_key)] @name) @def
(pair [(bare_key) (dotted_key) (quoted_key)] @name) @def

(table [(bare_key) (dotted_key) (quoted_key)] @scope.name) @scope
(pair [(bare_key) (dotted_key) (quoted_key)] @scope.name) @scope
";
