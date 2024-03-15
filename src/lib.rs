//! `tree_magic_mini` is a Rust crate that determines the MIME type a given file or byte stream.
//!
//! This is a fork of the [tree_magic](https://crates.io/crates/tree_magic)
//! crate by Allison Hancock. It includes the following changes:
//!
//! * Updated dependencies.
//! * Reduced copying and memory allocation, for a slight increase in speed and
//!   decrease in memory use.
//! * Reduced API surface. Some previously public APIs are now internal.
//! * Removed the optional `cli` feature and `tmagic` binary.
//!
//! # About tree_magic
//!
//! `tree_magic` is designed to be more efficient and to have less false positives compared
//! to the old approach used by `libmagic`, or old-fashioned file extension comparisons.
//!
//! Instead, this loads all known MIME types into a tree based on subclasses. Then, instead
//! of checking against *every* file type, `tree_magic` will traverse down the tree and
//! only check the files that make sense to check.
//!
//! # Features
//!
//! - Very fast perfomance (~150ns to check one file against one type,
//!   between 5,000ns and 100,000ns to find a MIME type.)
//! - Check if a file *is* a certain type.
//! - Handles aliases (ex: `application/zip` vs `application/x-zip-compressed`)
//! - Can delegate different file types to different "checkers", reducing false positives
//!   by choosing a different method of attack.
//!
//! ## Licensing and the MIME database
//!
//! By default, `tree_magic_mini` will attempt to load the shared MIME info
//! database from the standard locations at runtime.
//!
//! If you won't have the database files available, or would like to include them
//! in your binary for simplicity, you can optionally embed the database
//! information if you enable the `tree_magic_db` feature.
//!
//! **As the magic database files themselves are licensed under the GPL, you must
//! make sure your project uses a compatible license if you enable this behaviour.**
//!
//! # Example
//! ```rust
//! // Load a GIF file
//! let input: &[u8] = include_bytes!("../tests/image/gif");
//!
//! // Find the MIME type of the GIF
//! let result = tree_magic_mini::from_u8(input);
//! assert_eq!(result, "image/gif");
//!
//! // Check if the MIME and the file are a match
//! let result = tree_magic_mini::match_u8("image/gif", input);
//! assert_eq!(result, true);
//! ```

use fnv::{FnvHashMap, FnvHashSet};
use once_cell::sync::Lazy;
use petgraph::prelude::*;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;

mod basetype;
mod fdo_magic;

type Mime = &'static str;

/// Check these types first
/// TODO: Poll these from the checkers? Feels a bit arbitrary
const TYPEORDER: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "application/zip",
    "application/x-msdos-executable",
    "application/pdf",
];

trait Checker: Send + Sync {
    fn match_bytes(&self, bytes: &[u8], mimetype: &str) -> bool;
    fn match_file(&self, file: &File, mimetype: &str) -> bool;
    fn get_supported(&self) -> Vec<Mime>;
    fn get_subclasses(&self) -> Vec<(Mime, Mime)>;
    fn get_aliaslist(&self) -> FnvHashMap<Mime, Mime>;
}

static CHECKERS: &[&'static dyn Checker] = &[
    &fdo_magic::builtin::check::FdoMagic,
    &basetype::check::BaseType,
];

// Mappings between modules and supported mimes

static CHECKER_SUPPORT: Lazy<FnvHashMap<Mime, &'static dyn Checker>> = Lazy::new(|| {
    let mut out = FnvHashMap::<Mime, &'static dyn Checker>::default();
    for &c in CHECKERS {
        for m in c.get_supported() {
            out.insert(m, c);
        }
    }
    out
});

static ALIASES: Lazy<FnvHashMap<Mime, Mime>> = Lazy::new(|| {
    let mut out = FnvHashMap::<Mime, Mime>::default();
    for &c in CHECKERS {
        out.extend(c.get_aliaslist());
    }
    out
});

/// Information about currently loaded MIME types
///
/// The `graph` contains subclass relations between all given mimes.
/// (EX: `application/json` -> `text/plain` -> `application/octet-stream`)
/// This is a `petgraph` DiGraph, so you can walk the tree if needed.
///
/// The `hash` is a mapping between MIME types and nodes on the graph.
/// The root of the graph is "all/all", so start traversing there unless
/// you need to jump to a particular node.
struct TypeStruct {
    graph: DiGraph<Mime, u32>,
}

/// The TypeStruct autogenerated at library init, and used by the library.
static TYPE: Lazy<TypeStruct> = Lazy::new(|| {
    let mut graph = DiGraph::<Mime, u32>::new();
    let mut added_mimes = FnvHashMap::<Mime, NodeIndex>::default();

    // Get list of MIME types and MIME relations
    let mut mimelist = Vec::<Mime>::new();
    let mut edgelist_raw = Vec::<(Mime, Mime)>::new();
    for &c in CHECKERS {
        mimelist.extend(c.get_supported());
        edgelist_raw.extend(c.get_subclasses());
    }
    mimelist.sort_unstable();
    mimelist.dedup();
    let mimelist = mimelist;

    // Create all nodes
    for mimetype in mimelist.iter() {
        let node = graph.add_node(mimetype);
        added_mimes.insert(mimetype, node);
    }

    let mut edge_list = FnvHashSet::<(NodeIndex, NodeIndex)>::with_capacity_and_hasher(
        edgelist_raw.len(),
        Default::default(),
    );
    for (child_raw, parent_raw) in &edgelist_raw {
        let Some(parent) = added_mimes.get(parent_raw) else {
            continue;
        };
        let Some(child) = added_mimes.get(child_raw) else {
            continue;
        };
        edge_list.insert((*child, *parent));
    }

    graph.extend_with_edges(&edge_list);

    //Add to applicaton/octet-stream, all/all, or text/plain, depending on top-level
    //(We'll just do it here because having the graph makes it really nice)
    let node_text = *added_mimes
        .entry("text/plain")
        .or_insert_with(|| graph.add_node("text/plain"));

    let node_octet = *added_mimes
        .entry("application/octet-stream")
        .or_insert_with(|| graph.add_node("application/octet-stream"));

    let node_allall = *added_mimes
        .entry("all/all")
        .or_insert_with(|| graph.add_node("all/all"));

    let node_allfiles = *added_mimes
        .entry("all/allfiles")
        .or_insert_with(|| graph.add_node("all/allfiles"));

    let mut edge_list_2 = FnvHashSet::<(NodeIndex, NodeIndex)>::default();
    for mimenode in graph.externals(Incoming) {
        let mimetype = &graph[mimenode];
        let toplevel = mimetype.split('/').next().unwrap_or("");

        if mimenode == node_text
            || mimenode == node_octet
            || mimenode == node_allfiles
            || mimenode == node_allall
        {
            continue;
        }

        if toplevel == "text" {
            edge_list_2.insert((node_text, mimenode));
        } else if toplevel == "inode" {
            edge_list_2.insert((node_allall, mimenode));
        } else {
            edge_list_2.insert((node_octet, mimenode));
        }
    }
    // Don't add duplicate entries
    graph.extend_with_edges(edge_list_2.difference(&edge_list));

    TypeStruct { graph }
});

/// Just the part of from_*_node that walks the graph
fn typegraph_walker<T, F>(parentnode: NodeIndex, input: &T, matchfn: F) -> Option<Mime>
where
    T: ?Sized,
    F: Fn(&str, &T) -> bool,
{
    // Pull most common types towards top
    let mut children: Vec<NodeIndex> = TYPE
        .graph
        .neighbors_directed(parentnode, Outgoing)
        .collect();

    for i in 0..children.len() {
        let x = children[i];
        if TYPEORDER.contains(&TYPE.graph[x]) {
            children.remove(i);
            children.insert(0, x);
        }
    }

    // Walk graph
    for childnode in children {
        let mimetype = &TYPE.graph[childnode];

        let result = matchfn(mimetype, input);
        match result {
            true => match typegraph_walker(childnode, input, matchfn) {
                Some(foundtype) => return Some(foundtype),
                None => return Some(mimetype),
            },
            false => continue,
        }
    }

    None
}

/// Transforms an alias into it's real type
fn get_alias(mimetype: &str) -> &str {
    match ALIASES.get(mimetype) {
        Some(x) => x,
        None => mimetype,
    }
}

/// Internal function. Checks if an alias exists, and if it does,
/// then runs `match_bytes`.
fn match_u8_noalias(mimetype: &str, bytes: &[u8]) -> bool {
    match CHECKER_SUPPORT.get(mimetype) {
        None => false,
        Some(y) => y.match_bytes(bytes, mimetype),
    }
}

/// Checks if the given bytestream matches the given MIME type.
///
/// Returns true or false if it matches or not. If the given MIME type is not known,
/// the function will always return false.
/// If mimetype is an alias of a known MIME, the file will be checked agains that MIME.
///
/// # Examples
/// ```rust
/// // Load a GIF file
/// let input: &[u8] = include_bytes!("../tests/image/gif");
///
/// // Check if the MIME and the file are a match
/// let result = tree_magic_mini::match_u8("image/gif", input);
/// assert_eq!(result, true);
/// ```
pub fn match_u8(mimetype: &str, bytes: &[u8]) -> bool {
    match_u8_noalias(get_alias(mimetype), bytes)
}

/// Gets the type of a file from a raw bytestream, starting at a certain node
/// in the type graph.
///
/// Returns MIME as string wrapped in Some if a type matches, or
/// None if no match is found under the given node.
/// Retreive the node from the `TYPE.hash` HashMap, using the MIME as the key.
///
/// # Panics
/// Will panic if the given node is not found in the graph.
/// As the graph is immutable, this should not happen if the node index comes from
/// TYPE.hash.
fn from_u8_node(parentnode: NodeIndex, bytes: &[u8]) -> Option<Mime> {
    typegraph_walker(parentnode, bytes, match_u8_noalias)
}

/// Gets the type of a file from a byte stream.
///
/// Returns MIME as string.
///
/// # Examples
/// ```rust
/// // Load a GIF file
/// let input: &[u8] = include_bytes!("../tests/image/gif");
///
/// // Find the MIME type of the GIF
/// let result = tree_magic_mini::from_u8(input);
/// assert_eq!(result, "image/gif");
/// ```
pub fn from_u8(bytes: &[u8]) -> Mime {
    let node = match TYPE.graph.externals(Incoming).next() {
        Some(foundnode) => foundnode,
        None => panic!("No filetype definitions are loaded."),
    };
    from_u8_node(node, bytes).unwrap()
}

/// Check if the given file matches the given MIME type.
///
/// # Examples
/// ```rust
/// use std::fs::File;
///
/// // Get path to a GIF file
/// let file = File::open("tests/image/gif").unwrap();
///
/// // Check if the MIME and the file are a match
/// let result = tree_magic_mini::match_file("image/gif", &file);
/// assert_eq!(result, true);
/// ```
pub fn match_file(mimetype: &str, file: &File) -> bool {
    match_file_noalias(get_alias(mimetype), file)
}

/// Internal function. Checks if an alias exists, and if it does,
/// then runs `match_file`.
fn match_file_noalias(mimetype: &str, file: &File) -> bool {
    match CHECKER_SUPPORT.get(mimetype) {
        None => false,
        Some(c) => c.match_file(file, mimetype),
    }
}

/// Check if the file at the given path matches the given MIME type.
///
/// Returns false if the file could not be read or the given MIME type is not known.
///
/// # Examples
/// ```rust
/// use std::path::Path;
///
/// // Get path to a GIF file
/// let path: &Path = Path::new("tests/image/gif");
///
/// // Check if the MIME and the file are a match
/// let result = tree_magic_mini::match_filepath("image/gif", path);
/// assert_eq!(result, true);
/// ```
#[inline]
pub fn match_filepath(mimetype: &str, path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    match_file(mimetype, &file)
}

/// Gets the type of a file, starting at a certain node in the type graph.
fn from_file_node(parentnode: NodeIndex, file: &File) -> Option<Mime> {
    // We're actually just going to thunk this down to a u8
    // unless we're checking via basetype for speed reasons.

    // Ensure it's at least a application/octet-stream
    if !match_file("application/octet-stream", file) {
        // Check the other base types
        return typegraph_walker(parentnode, file, match_file_noalias);
    }

    // Load the first 2K of file and parse as u8
    // for batch processing like this
    let bytes = read_bytes(file, 2048).ok()?;
    from_u8_node(parentnode, &bytes)
}

/// Gets the MIME type of a file.
///
/// Does not look at file name or extension, just the contents.
///
/// # Examples
/// ```rust
/// use std::fs::File;
///
/// // Get path to a GIF file
/// let file = File::open("tests/image/gif").unwrap();
///
/// // Find the MIME type of the GIF
/// let result = tree_magic_mini::from_file(&file);
/// assert_eq!(result, Some("image/gif"));
/// ```
pub fn from_file(file: &File) -> Option<Mime> {
    let node = TYPE.graph.externals(Incoming).next()?;
    from_file_node(node, file)
}

/// Gets the MIME type of a file.
///
/// Does not look at file name or extension, just the contents.
/// Returns None if the file is cannot be opened
/// or if no matching MIME type is found.
///
/// # Examples
/// ```rust
/// use std::path::Path;
///
/// // Get path to a GIF file
/// let path = Path::new("tests/image/gif");
///
/// // Find the MIME type of the GIF
/// let result = tree_magic_mini::from_filepath(path);
/// assert_eq!(result, Some("image/gif"));
/// ```
#[inline]
pub fn from_filepath(path: &Path) -> Option<Mime> {
    let file = File::open(path).ok()?;
    from_file(&file)
}

/// Reads the given number of bytes from a file
fn read_bytes(file: &File, bytecount: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::<u8>::with_capacity(bytecount);
    file.take(bytecount as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}
