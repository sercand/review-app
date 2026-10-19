//! The file tree the left pane draws (spec §9.2).
//!
//! [`build_tree`] folds a flat list of files into a directory tree: directories
//! precede files and both are ordered by name, single-child directory chains
//! collapse into one row (`a/b/c`), and each directory rolls up the `+`/`-`
//! counts of its descendants. A [`TreeNode`] keeps the index of the file it
//! stands for (into the slice [`build_tree`] was given), so rendering never
//! clones a path back out of the tree.

use crate::github::files::PrFile;

/// One row of the file tree: a directory or a file.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    /// The display label; a collapsed chain keeps its slashes (`"a/b/c"`).
    pub name: String,
    /// The full path from the repository root (`""` for the tree's root).
    pub path: String,
    /// Whether the node is a directory.
    pub is_dir: bool,
    /// Lines added under this node, rolled up for directories.
    pub additions: u32,
    /// Lines deleted under this node, rolled up for directories.
    pub deletions: u32,
    /// The node's children, directories first and each sorted by name.
    pub children: Vec<TreeNode>,
    /// The index of the file in the slice passed to [`build_tree`].
    pub file_index: Option<usize>,
}

/// Build the tree for the files `include` accepts.
///
/// The returned root has `path == ""` and is never collapsed — its `children`
/// are the top-level entries. `file_index` indexes the original slice, so
/// callers resolve a row back to the same `PrFile` they passed in.
pub fn build_tree(files: &[PrFile], include: &dyn Fn(&PrFile) -> bool) -> TreeNode {
    let mut root = Raw::dir("", "");
    for (index, file) in files.iter().enumerate() {
        if include(file) {
            root.insert(file, index);
        }
    }
    root.finish(false)
}

/// A node mid-build: children are unordered until [`Raw::finish`].
struct Raw {
    name: String,
    path: String,
    is_dir: bool,
    additions: u32,
    deletions: u32,
    file_index: Option<usize>,
    children: Vec<Raw>,
}

impl Raw {
    /// An empty directory node.
    fn dir(name: &str, path: &str) -> Raw {
        Raw {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: true,
            additions: 0,
            deletions: 0,
            file_index: None,
            children: Vec::new(),
        }
    }

    /// A file node carrying its own counts and original index.
    fn file(name: &str, path: &str, index: usize, file: &PrFile) -> Raw {
        Raw {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: false,
            additions: file.additions,
            deletions: file.deletions,
            file_index: Some(index),
            children: Vec::new(),
        }
    }

    /// The child directory `name` under `path`, creating it when absent.
    fn child_dir(&mut self, name: &str, path: &str) -> &mut Raw {
        match self
            .children
            .iter()
            .position(|child| child.is_dir && child.name == name)
        {
            Some(index) => &mut self.children[index],
            None => {
                self.children.push(Raw::dir(name, path));
                self.children
                    .last_mut()
                    .expect("a just-pushed child is present")
            }
        }
    }

    /// Insert one file, splitting its path on `/`.
    fn insert(&mut self, file: &PrFile, index: usize) {
        let mut segments = file.path.split('/');
        let last = segments.next_back().unwrap_or(&file.path);

        let mut node = self;
        let mut path = String::new();
        for segment in segments {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
            node = node.child_dir(segment, &path);
        }

        let full = if path.is_empty() {
            last.to_owned()
        } else {
            format!("{path}/{last}")
        };
        node.children.push(Raw::file(last, &full, index, file));
    }

    /// Sort, roll up, and collapse into the finished [`TreeNode`].
    ///
    /// `collapse` merges a chain of single-directory children (`a` + `b` →
    /// `"a/b"`); it is off for the root, whose children are the top level.
    fn finish(mut self, collapse: bool) -> TreeNode {
        self.children
            .sort_by(|a, b| (!a.is_dir, a.name.as_str()).cmp(&(!b.is_dir, b.name.as_str())));

        if !self.is_dir {
            return TreeNode {
                name: self.name,
                path: self.path,
                is_dir: false,
                additions: self.additions,
                deletions: self.deletions,
                children: Vec::new(),
                file_index: self.file_index,
            };
        }

        let mut name = self.name;
        let mut path = self.path;
        let mut children: Vec<TreeNode> = self
            .children
            .into_iter()
            .map(|child| child.finish(true))
            .collect();

        while collapse && children.len() == 1 && children[0].is_dir {
            let child = children.pop().expect("a single child is present");
            name = if name.is_empty() {
                child.name
            } else {
                format!("{name}/{}", child.name)
            };
            path = child.path;
            children = child.children;
        }

        let additions = children.iter().map(|child| child.additions).sum();
        let deletions = children.iter().map(|child| child.deletions).sum();

        TreeNode {
            name,
            path,
            is_dir: true,
            additions,
            deletions,
            children,
            file_index: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{files_at, find, tree_at};

    #[test]
    fn nests_sorts_rolls_up_and_collapses_dir_chains() {
        let root = tree_at(&[
            "src/state/model.rs",
            "src/ui/app.rs",
            "README.md",
            "a/b/c/x.rs",
        ]);
        let names: Vec<_> = root.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["a/b/c", "src", "README.md"]); // dirs first; chain collapsed
        assert_eq!(find(&root, "a/b/c").unwrap().children[0].name, "x.rs");
        let state = find(&root, "src/state").unwrap(); // src has two children: not collapsed
        assert!(state.is_dir && state.children[0].name == "model.rs");
        assert_eq!(find(&root, "README.md").unwrap().file_index, Some(2));
        assert_eq!(find(&root, "src").unwrap().additions, 2); // 1 per file from `file()`
    }

    #[test]
    fn filter_excludes_files_from_tree() {
        let files = files_at(&["src/a.rs", "docs/x.md"]);
        let root = build_tree(&files, &|f| f.path.ends_with(".rs"));
        assert!(find(&root, "docs").is_none());
        assert!(find(&root, "src/a.rs").is_some());
    }
}
