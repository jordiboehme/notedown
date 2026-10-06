//! Following addresses from the current footer, with bounds checks and a record
//! of every reached block (docs/note-format.md 2, "Finding the current state"; 10).

use std::collections::BTreeSet;

use crate::container;
use crate::error::{NoteError, corrupt};
use crate::meta::Meta;
use crate::model::BlockRef;
use crate::values::{self, Warnings};

pub(crate) struct Resolver<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) first_block: u64,
    pub(crate) footer_address: u64,
    pub(crate) file_type: String,
    pub(crate) reachable: BTreeSet<u64>,
    pub(crate) warnings: Warnings,
}

impl<'a> Resolver<'a> {
    /// Checks the block at `address` and marks it reachable. A reference back to
    /// the footer is corrupt (docs/note-format.md 10.4).
    pub(crate) fn reference(&mut self, what: &str, address: u64) -> Result<BlockRef, NoteError> {
        if address == self.footer_address {
            return Err(corrupt(format!(
                "{what}: points at the footer block {address}"
            )));
        }
        let body = container::payload(self.data, self.first_block, address)
            .map_err(|e| corrupt(format!("{what}: {e}")))?;
        self.reachable.insert(address);
        Ok(BlockRef {
            address,
            len: body.len(),
        })
    }

    /// The payload of a block already checked by `reference`.
    pub(crate) fn payload(&self, block: BlockRef) -> &'a [u8] {
        container::payload(self.data, self.first_block, block.address).unwrap_or_default()
    }

    /// An address value that must resolve to a block (strict).
    pub(crate) fn address_value(
        &mut self,
        what: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<BlockRef>, NoteError> {
        match values::address(what, key, value)? {
            None => Ok(None),
            Some(a) => self.reference(&format!("{what} {key}"), a).map(Some),
        }
    }

    /// The address under `key` in `meta` (strict).
    pub(crate) fn address(
        &mut self,
        what: &str,
        meta: &Meta,
        key: &str,
    ) -> Result<Option<BlockRef>, NoteError> {
        match meta.get(key) {
            None => Ok(None),
            Some(value) => self.address_value(what, key, value),
        }
    }

    /// An address-shaped key whose content is unknown (docs/note-format.md 12):
    /// a value that does not resolve gives a warning, not an error.
    pub(crate) fn soft_address(&mut self, what: &str, meta: &Meta, key: &str) -> Option<BlockRef> {
        match self.address(what, meta, key) {
            Ok(r) => r,
            Err(e) => {
                self.warnings.push(e.to_string());
                None
            }
        }
    }

    /// Parses the metadata block at `block` (strict).
    pub(crate) fn metadata(&self, what: &str, block: BlockRef) -> Result<Meta, NoteError> {
        Meta::parse(self.payload(block))
            .map_err(|e| corrupt(format!("{what}: metadata block at {}: {e}", block.address)))
    }
}
