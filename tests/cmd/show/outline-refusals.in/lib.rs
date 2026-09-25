use std::path::Path;

pub struct Store {
    root: String,
}

impl Store {
    pub fn open(path: &Path) -> Store {
        Store {
            root: path.display().to_string(),
        }
    }
}

pub fn wrapped(first: usize, second: usize) -> usize {
    first + second
}
