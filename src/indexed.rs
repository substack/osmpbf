//! Speed up searches by using an index

use error::Result;
use std::fs::File;
use std::io::{Read, Seek};
use std::ops::RangeInclusive;
use std::path::Path;
use {BlobReader, BlobType, ByteOffset, Element, Way};

/// Stores the minimum and maximum id of every element type.
#[derive(Debug)]
pub struct IdRanges {
    node_ids: Option<RangeInclusive<i64>>,
    way_ids: Option<RangeInclusive<i64>>,
    relation_ids: Option<RangeInclusive<i64>>,
}

/// Checks if `sorted_slice` contains some values from the given `range`.
/// Assumes that `sorted_slice` is sorted.
/// Returns the range of indices into `sorted_slice` that needs to be checked.
/// Returns `None` if it is guaranteed that no values from `sorted_slice` are inside `range`.
fn range_included(range: &RangeInclusive<i64>, sorted_slice: &[i64]) -> Option<RangeInclusive<usize>> {
    match (sorted_slice.binary_search(&range.start()), sorted_slice.binary_search(&range.end())) {
        (Ok(start), Ok(end)) => Some(RangeInclusive::new(start, end)),
        (Ok(start), Err(end)) => Some(RangeInclusive::new(start, end.saturating_sub(1))),
        (Err(start), Ok(end)) => Some(RangeInclusive::new(start, end)),
        (Err(start), Err(end)) => {
            if start == end {
                None
            } else {
                Some(RangeInclusive::new(start, end.saturating_sub(1)))
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SimpleBlobType {
    Header,
    Primitive,
    Unknown,
}

#[derive(Debug)]
struct BlobInfo {
    offset: ByteOffset,
    blob_type: SimpleBlobType,
    id_ranges: Option<IdRanges>,
}

/// Allows filtering elements and iterating over their dependencies.
/// It chooses an efficient method for navigating the PBF structure to achieve this in reasonable
/// time and with reasonable memory.
pub struct IndexedReader<R: Read + Seek> {
    reader: BlobReader<R>,
    index: Vec<BlobInfo>,
}

impl<R: Read + Seek> IndexedReader<R> {
    /// Creates a new `IndexedReader`.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let reader = IndexedReader::new(buf_reader)?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(reader: R) -> Result<Self> {
        let reader = BlobReader::new_seekable(reader)?;
        Ok(Self {
            reader,
            index: vec![],
        })
    }

    fn create_index(&mut self) -> Result<()> {
        // remove old items
        self.index.clear();

        for blob in &mut self.reader {
            let blob = blob?;
            // Reader is seekable, so offset should return Some(ByteOffset)
            let offset = blob.offset().unwrap();
            let blob_type = match blob.get_type() {
                BlobType::OsmHeader => SimpleBlobType::Header,
                BlobType::OsmData => SimpleBlobType::Primitive,
                BlobType::Unknown(_) => SimpleBlobType::Unknown,
            };

            self.index.push(BlobInfo {
                offset,
                blob_type,
                id_ranges: None,
            });
        }

        Ok(())
    }

    /// Filter ways using a closure and return matching ways and their dependent nodes (`Node`s and
    /// `DenseNode`s) in another closure.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = IndexedReader::from_path("tests/test.osm.pbf")?;
    /// let mut ways = 0;
    /// let mut nodes = 0;
    ///
    /// // Filter all ways that are buildings and count their nodes.
    /// reader.read_ways_and_deps(
    ///     |way| {
    ///         // Filter ways. Return true if tags contain "building": "yes".
    ///         way.tags().any(|key_value| key_value == ("building", "yes"))
    ///     },
    ///     |element| {
    ///         // Increment counter
    ///         match element {
    ///             Element::Way(way) => ways += 1,
    ///             Element::Node(node) => nodes += 1,
    ///             Element::DenseNode(dense_node) => nodes += 1,
    ///             Element::Relation(_) => (), // should not occur
    ///         }
    ///     },
    /// )?;
    ///
    /// println!("ways:  {}\nnodes: {}", ways, nodes);
    ///
    /// # assert_eq!(ways, 1);
    /// # assert_eq!(nodes, 3);
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn read_ways_and_deps<F, E>(
        &mut self,
        mut filter: F,
        mut element_callback: E,
    ) -> Result<()>
    where
        F: for<'a> FnMut(&Way<'a>) -> bool,
        E: for<'a> FnMut(&Element<'a>),
    {
        // Create index
        if self.index.is_empty() {
            self.create_index()?;
        }

        let mut node_ids: Vec<i64> = vec![];

        // First pass:
        //   * Filter ways and store their dependencies as node IDs
        //   * Store range of node IDs (min and max value) of each block
        for info in &mut self.index {
            //TODO do something useful with header blocks
            if info.blob_type == SimpleBlobType::Primitive {
                self.reader.seek(info.offset)?;
                let blob = self.reader.next().ok_or_else(|| {
                    ::std::io::Error::new(
                        ::std::io::ErrorKind::UnexpectedEof,
                        "could not read next blob",
                    )
                })??;
                let block = blob.to_primitiveblock()?;
                let mut min_node_id: Option<i64> = None;
                let mut max_node_id: Option<i64> = None;
                for group in block.groups() {
                    // filter ways and record node IDs
                    for way in group.ways() {
                        if filter(&way) {
                            let refs = way.refs();

                            node_ids.reserve(refs.size_hint().0);
                            for node_id in refs {
                                node_ids.push(node_id);
                            }

                            // Return way
                            element_callback(&Element::Way(way));
                        }
                    }

                    // Check node IDs of this block, record min and max

                    let mut check_min_max = |id| {
                        min_node_id = Some(min_node_id.map_or(id, |x| x.min(id)));
                        max_node_id = Some(max_node_id.map_or(id, |x| x.max(id)));
                    };

                    for node in group.nodes() {
                        check_min_max(node.id())
                    }
                    for node in group.dense_nodes() {
                        check_min_max(node.id)
                    }
                }
                if let (Some(min), Some(max)) = (min_node_id, max_node_id) {
                    info.id_ranges = Some(IdRanges {
                        node_ids: Some(RangeInclusive::new(min, max)),
                        way_ids: None,
                        relation_ids: None,
                    });
                }
            }
        }

        // Sort, to enable binary search
        node_ids.sort_unstable();

        // Remove duplicate node IDs
        node_ids.dedup();

        // Second pass:
        //   * Iterate only over blobs that may include the node IDs we're searching for
        for info in &mut self.index {
            if info.blob_type == SimpleBlobType::Primitive {
                if let Some(node_id_range) = info.id_ranges.as_ref().and_then(|r| r.node_ids.as_ref()) {
                    if let Some(slice_range) = range_included(node_id_range, &node_ids) {
                        let ids_subslice = &node_ids.as_slice()[slice_range];

                        self.reader.seek(info.offset)?;
                        let blob = self.reader.next().ok_or_else(|| {
                            ::std::io::Error::new(
                                ::std::io::ErrorKind::UnexpectedEof,
                                "could not read next blob",
                            )
                        })??;
                        let block = blob.to_primitiveblock()?;
                        for group in block.groups() {
                            for node in group.nodes() {
                                let id = node.id();
                                if ids_subslice.binary_search(&id).is_ok() {
                                    // ID found, return node
                                    element_callback(&Element::Node(node));
                                }
                            }
                            for node in group.dense_nodes() {
                                let id = node.id;
                                if ids_subslice.binary_search(&id).is_ok() {
                                    // ID found, return dense node
                                    element_callback(&Element::DenseNode(node));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl IndexedReader<File> {
    /// Creates a new `IndexedReader` from a given path.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = IndexedReader::from_path("tests/test.osm.pbf")?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        //TODO take some more measurements to determine if `BufReader` should be used here
        let f = File::open(path)?;
        Self::new(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_included() {
        assert_eq!(range_included(&RangeInclusive::new(0, 0), &[1,2,3]), None);
        assert_eq!(range_included(&RangeInclusive::new(1, 1), &[1,2,3]), Some(RangeInclusive::new(0, 0)));
        assert_eq!(range_included(&RangeInclusive::new(2, 2), &[1,2,3]), Some(RangeInclusive::new(1, 1)));
        assert_eq!(range_included(&RangeInclusive::new(3, 3), &[1,2,3]), Some(RangeInclusive::new(2, 2)));
        assert_eq!(range_included(&RangeInclusive::new(4, 4), &[1,2,3]), None);
        assert_eq!(range_included(&RangeInclusive::new(0, 1), &[1,2,3]), Some(RangeInclusive::new(0, 0)));
        assert_eq!(range_included(&RangeInclusive::new(3, 4), &[1,2,3]), Some(RangeInclusive::new(2, 2)));
        assert_eq!(range_included(&RangeInclusive::new(4, 4), &[1,2,6]), None);
        assert_eq!(range_included(&RangeInclusive::new(2, 3), &[1,2,6]), Some(RangeInclusive::new(1, 1)));
        assert_eq!(range_included(&RangeInclusive::new(5, 6), &[1,2,6]), Some(RangeInclusive::new(2, 2)));
        assert_eq!(range_included(&RangeInclusive::new(5, 8), &[1,2,6]), Some(RangeInclusive::new(2, 2)));
        assert_eq!(range_included(&RangeInclusive::new(0, 8), &[1,2,6]), Some(RangeInclusive::new(0, 2)));
        assert_eq!(range_included(&RangeInclusive::new(0, 4), &[1,2,6]), Some(RangeInclusive::new(0, 1)));
    }
}