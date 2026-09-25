package store

type Store struct {
	root string
}

func Open(
	path string,
) (*Store, error) {
	return &Store{root: path}, nil
}

func (s *Store) Root() string {
	return s.root
}
