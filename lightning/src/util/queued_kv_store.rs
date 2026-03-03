// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! A [`KVStoreSync`] wrapper that queues writes and removes until explicitly committed.
//!
//! This is useful for batching multiple monitor updates into a single persistence operation,
//! reducing I/O overhead.

use crate::io;
use crate::io::Read;
use crate::prelude::*;
use crate::sync::Mutex;
use crate::util::persist::{
	KVStoreSync, SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
	SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::util::ser::{Readable, Writeable, Writer};

use core::ops::Deref;
use core::sync::atomic::{AtomicU64, Ordering};

/// A [`KVStoreSync`] wrapper that queues writes and removes until explicitly committed.
///
/// This is useful for batching multiple monitor updates into a single persistence operation,
/// reducing I/O overhead.
///
/// Reads check pending operations first, then fall back to the inner store.
/// Writes and removes are queued and only applied when [`commit`] is called.
/// Multiple writes to the same key are deduplicated, keeping only the latest operation.
///
/// [`commit`]: Self::commit
pub struct QueuedKVStoreSync<K: Deref>
where
	K::Target: KVStoreSync,
{
	inner: K,
	pending_ops: Mutex<Vec<PendingOp>>,
	sequence_number: AtomicU64,
}

#[derive(Clone)]
enum PendingOp {
	Write { primary_namespace: String, secondary_namespace: String, key: String, value: Vec<u8> },
	Remove { primary_namespace: String, secondary_namespace: String, key: String, lazy: bool },
}

impl PendingOp {
	fn key_tuple(&self) -> (&str, &str, &str) {
		match self {
			PendingOp::Write { primary_namespace, secondary_namespace, key, .. } => {
				(primary_namespace, secondary_namespace, key)
			},
			PendingOp::Remove { primary_namespace, secondary_namespace, key, .. } => {
				(primary_namespace, secondary_namespace, key)
			},
		}
	}
}

const PENDING_OP_WRITE_TYPE: u8 = 0;
const PENDING_OP_REMOVE_TYPE: u8 = 1;

impl Writeable for PendingOp {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		match self {
			PendingOp::Write { primary_namespace, secondary_namespace, key, value } => {
				PENDING_OP_WRITE_TYPE.write(writer)?;
				primary_namespace.write(writer)?;
				secondary_namespace.write(writer)?;
				key.write(writer)?;
				value.write(writer)?;
			},
			PendingOp::Remove { primary_namespace, secondary_namespace, key, lazy } => {
				PENDING_OP_REMOVE_TYPE.write(writer)?;
				primary_namespace.write(writer)?;
				secondary_namespace.write(writer)?;
				key.write(writer)?;
				lazy.write(writer)?;
			},
		}
		Ok(())
	}
}

impl Readable for PendingOp {
	fn read<R: Read>(reader: &mut R) -> Result<Self, crate::ln::msgs::DecodeError> {
		let op_type: u8 = Readable::read(reader)?;
		match op_type {
			PENDING_OP_WRITE_TYPE => Ok(PendingOp::Write {
				primary_namespace: Readable::read(reader)?,
				secondary_namespace: Readable::read(reader)?,
				key: Readable::read(reader)?,
				value: Readable::read(reader)?,
			}),
			PENDING_OP_REMOVE_TYPE => Ok(PendingOp::Remove {
				primary_namespace: Readable::read(reader)?,
				secondary_namespace: Readable::read(reader)?,
				key: Readable::read(reader)?,
				lazy: Readable::read(reader)?,
			}),
			_ => Err(crate::ln::msgs::DecodeError::InvalidValue),
		}
	}
}

impl<K: Deref> QueuedKVStoreSync<K>
where
	K::Target: KVStoreSync,
{
	/// Creates a new `QueuedKVStoreSync` wrapping the given store.
	pub fn new(inner: K) -> Self {
		Self { inner, pending_ops: Mutex::new(Vec::new()), sequence_number: AtomicU64::new(0) }
	}

	/// Returns the number of pending operations.
	pub fn pending_count(&self) -> usize {
		self.pending_ops.lock().unwrap().len()
	}

	/// Clears all pending operations without committing.
	pub fn clear(&self) {
		self.pending_ops.lock().unwrap().clear();
	}

	/// Adds an operation, removing any previous ops for the same key.
	fn push_op(&self, op: PendingOp) {
		let mut pending = self.pending_ops.lock().unwrap();
		let new_key = op.key_tuple();
		pending.retain(|existing| existing.key_tuple() != new_key);
		pending.push(op);
	}

	/// Finds the pending op for a key, if any.
	fn find_pending(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Option<PendingOp> {
		let pending = self.pending_ops.lock().unwrap();
		pending
			.iter()
			.find(|op| op.key_tuple() == (primary_namespace, secondary_namespace, key))
			.cloned()
	}
}

impl<K: Deref> KVStoreSync for QueuedKVStoreSync<K>
where
	K::Target: KVStoreSync,
{
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Result<Vec<u8>, io::Error> {
		// Check pending ops first
		if let Some(op) = self.find_pending(primary_namespace, secondary_namespace, key) {
			return match op {
				PendingOp::Write { value, .. } => Ok(value),
				PendingOp::Remove { .. } => {
					Err(io::Error::new(io::ErrorKind::NotFound, "key pending removal"))
				},
			};
		}

		self.inner.read(primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Result<(), io::Error> {
		self.push_op(PendingOp::Write {
			primary_namespace: primary_namespace.to_string(),
			secondary_namespace: secondary_namespace.to_string(),
			key: key.to_string(),
			value: buf,
		});
		Ok(())
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Result<(), io::Error> {
		self.push_op(PendingOp::Remove {
			primary_namespace: primary_namespace.to_string(),
			secondary_namespace: secondary_namespace.to_string(),
			key: key.to_string(),
			lazy,
		});
		Ok(())
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Result<Vec<String>, io::Error> {
		// Get from inner store
		let mut keys = self.inner.list(primary_namespace, secondary_namespace)?;

		// Apply pending ops
		let pending = self.pending_ops.lock().unwrap();
		for op in pending.iter() {
			match op {
				PendingOp::Write {
					primary_namespace: pn, secondary_namespace: sn, key, ..
				} if pn == primary_namespace && sn == secondary_namespace => {
					if !keys.contains(key) {
						keys.push(key.clone());
					}
				},
				PendingOp::Remove {
					primary_namespace: pn, secondary_namespace: sn, key, ..
				} if pn == primary_namespace && sn == secondary_namespace => {
					keys.retain(|k| k != key);
				},
				_ => {},
			}
		}

		Ok(keys)
	}

	fn commit(&self) -> Result<usize, io::Error> {
		let mut pending = self.pending_ops.lock().unwrap();
		let count = pending.len();
		if count == 0 {
			return Ok(0);
		}

		println!("QueuedKVStoreSync::commit: {} ops queued", count);
		for op in pending.iter() {
			match op {
				PendingOp::Write { primary_namespace, secondary_namespace, key, value } => {
					println!(
						"  WRITE {}/{}/{} ({} bytes)",
						primary_namespace,
						secondary_namespace,
						key,
						value.len()
					);
				},
				PendingOp::Remove { primary_namespace, secondary_namespace, key, lazy } => {
					println!(
						"  REMOVE {}/{}/{} (lazy={})",
						primary_namespace, secondary_namespace, key, lazy
					);
				},
			}
		}

		let mut buf = Vec::new();
		(count as u64).write(&mut buf).expect("Vec write cannot fail");
		for op in pending.iter() {
			op.write(&mut buf).expect("Vec write cannot fail");
		}

		println!("  total delta size: {} bytes", buf.len());

		let seq = self.sequence_number.fetch_add(1, Ordering::Relaxed);
		let key = seq.to_string();
		self.inner.write(
			SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
			SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
			&key,
			buf,
		)?;

		pending.clear();
		Ok(count)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::RwLock;
	use std::collections::HashMap;

	/// A simple in-memory KVStore for testing.
	struct TestStore {
		data: RwLock<HashMap<(String, String, String), Vec<u8>>>,
	}

	impl TestStore {
		fn new() -> Self {
			Self { data: RwLock::new(HashMap::new()) }
		}
	}

	impl KVStoreSync for TestStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> Result<Vec<u8>, io::Error> {
			let data = self.data.read().unwrap();
			data.get(&(
				primary_namespace.to_string(),
				secondary_namespace.to_string(),
				key.to_string(),
			))
			.cloned()
			.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "key not found"))
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> Result<(), io::Error> {
			let mut data = self.data.write().unwrap();
			data.insert(
				(primary_namespace.to_string(), secondary_namespace.to_string(), key.to_string()),
				buf,
			);
			Ok(())
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
		) -> Result<(), io::Error> {
			let mut data = self.data.write().unwrap();
			data.remove(&(
				primary_namespace.to_string(),
				secondary_namespace.to_string(),
				key.to_string(),
			));
			Ok(())
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> Result<Vec<String>, io::Error> {
			let data = self.data.read().unwrap();
			let keys: Vec<String> = data
				.keys()
				.filter(|(pn, sn, _)| pn == primary_namespace && sn == secondary_namespace)
				.map(|(_, _, k)| k.clone())
				.collect();
			Ok(keys)
		}
	}

	fn read_delta(store: &TestStore, key: &str) -> Vec<PendingOp> {
		let buf = store
			.read(
				SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
				SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
				key,
			)
			.unwrap();
		let mut reader = &buf[..];
		let count: u64 = Readable::read(&mut reader).unwrap();
		let mut ops = Vec::new();
		for _ in 0..count {
			ops.push(Readable::read(&mut reader).unwrap());
		}
		ops
	}

	#[test]
	fn test_write_queued_until_commit() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		queued.write("ns", "", "key1", vec![1, 2, 3]).unwrap();
		assert_eq!(queued.pending_count(), 1);
		assert!(inner.read("ns", "", "key1").is_err());

		assert_eq!(queued.read("ns", "", "key1").unwrap(), vec![1, 2, 3]);

		let committed = queued.commit().unwrap();
		assert_eq!(committed, 1);
		assert_eq!(queued.pending_count(), 0);

		let ops = read_delta(&inner, "0");
		assert_eq!(ops.len(), 1);
		match &ops[0] {
			PendingOp::Write { primary_namespace, key, value, .. } => {
				assert_eq!(primary_namespace, "ns");
				assert_eq!(key, "key1");
				assert_eq!(value, &vec![1, 2, 3]);
			},
			_ => panic!("expected Write op"),
		}
	}

	#[test]
	fn test_remove_queued_until_commit() {
		let inner = TestStore::new();
		inner.write("ns", "", "key1", vec![1, 2, 3]).unwrap();

		let queued = QueuedKVStoreSync::new(&inner);

		queued.remove("ns", "", "key1", false).unwrap();
		assert_eq!(queued.pending_count(), 1);
		assert!(inner.read("ns", "", "key1").is_ok());

		assert!(queued.read("ns", "", "key1").is_err());

		queued.commit().unwrap();

		let ops = read_delta(&inner, "0");
		assert_eq!(ops.len(), 1);
		match &ops[0] {
			PendingOp::Remove { primary_namespace, key, lazy, .. } => {
				assert_eq!(primary_namespace, "ns");
				assert_eq!(key, "key1");
				assert!(!lazy);
			},
			_ => panic!("expected Remove op"),
		}
	}

	#[test]
	fn test_deduplication_keeps_latest() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		queued.write("ns", "", "key1", vec![1]).unwrap();
		queued.write("ns", "", "key1", vec![2]).unwrap();
		queued.write("ns", "", "key1", vec![3]).unwrap();

		assert_eq!(queued.pending_count(), 1);
		assert_eq!(queued.read("ns", "", "key1").unwrap(), vec![3]);

		queued.commit().unwrap();
		let ops = read_delta(&inner, "0");
		assert_eq!(ops.len(), 1);
		match &ops[0] {
			PendingOp::Write { value, .. } => assert_eq!(value, &vec![3]),
			_ => panic!("expected Write op"),
		}
	}

	#[test]
	fn test_write_then_remove_deduplicates() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		// Write then remove same key
		queued.write("ns", "", "key1", vec![1, 2, 3]).unwrap();
		queued.remove("ns", "", "key1", false).unwrap();

		// Should only have one pending op (the remove)
		assert_eq!(queued.pending_count(), 1);

		// Read should return NotFound
		assert!(queued.read("ns", "", "key1").is_err());
	}

	#[test]
	fn test_remove_then_write_deduplicates() {
		let inner = TestStore::new();
		inner.write("ns", "", "key1", vec![0]).unwrap();

		let queued = QueuedKVStoreSync::new(&inner);

		// Remove then write same key
		queued.remove("ns", "", "key1", false).unwrap();
		queued.write("ns", "", "key1", vec![1, 2, 3]).unwrap();

		// Should only have one pending op (the write)
		assert_eq!(queued.pending_count(), 1);

		// Read should return the new value
		assert_eq!(queued.read("ns", "", "key1").unwrap(), vec![1, 2, 3]);
	}

	#[test]
	fn test_list_includes_pending_writes() {
		let inner = TestStore::new();
		inner.write("ns", "", "existing", vec![0]).unwrap();

		let queued = QueuedKVStoreSync::new(&inner);
		queued.write("ns", "", "new_key", vec![1]).unwrap();

		let keys = queued.list("ns", "").unwrap();
		assert!(keys.contains(&"existing".to_string()));
		assert!(keys.contains(&"new_key".to_string()));
	}

	#[test]
	fn test_list_excludes_pending_removes() {
		let inner = TestStore::new();
		inner.write("ns", "", "key1", vec![1]).unwrap();
		inner.write("ns", "", "key2", vec![2]).unwrap();

		let queued = QueuedKVStoreSync::new(&inner);
		queued.remove("ns", "", "key1", false).unwrap();

		let keys = queued.list("ns", "").unwrap();
		assert!(!keys.contains(&"key1".to_string()));
		assert!(keys.contains(&"key2".to_string()));
	}

	#[test]
	fn test_clear_discards_pending() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		queued.write("ns", "", "key1", vec![1]).unwrap();
		queued.write("ns", "", "key2", vec![2]).unwrap();
		assert_eq!(queued.pending_count(), 2);

		queued.clear();
		assert_eq!(queued.pending_count(), 0);

		// Inner should not have any data
		assert!(inner.read("ns", "", "key1").is_err());
		assert!(inner.read("ns", "", "key2").is_err());
	}

	#[test]
	fn test_multiple_namespaces() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		// Same key in different namespaces should not deduplicate
		queued.write("ns1", "", "key", vec![1]).unwrap();
		queued.write("ns2", "", "key", vec![2]).unwrap();
		queued.write("ns1", "sub", "key", vec![3]).unwrap();

		assert_eq!(queued.pending_count(), 3);

		assert_eq!(queued.read("ns1", "", "key").unwrap(), vec![1]);
		assert_eq!(queued.read("ns2", "", "key").unwrap(), vec![2]);
		assert_eq!(queued.read("ns1", "sub", "key").unwrap(), vec![3]);
	}

	#[test]
	fn test_read_falls_back_to_inner() {
		let inner = TestStore::new();
		inner.write("ns", "", "inner_key", vec![1, 2, 3]).unwrap();

		let queued = QueuedKVStoreSync::new(&inner);

		// Should read from inner when no pending op
		assert_eq!(queued.read("ns", "", "inner_key").unwrap(), vec![1, 2, 3]);
	}

	#[test]
	fn test_commit_empty_is_noop() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		let committed = queued.commit().unwrap();
		assert_eq!(committed, 0);

		let delta_keys = inner
			.list(
				SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
				SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			.unwrap();
		assert!(delta_keys.is_empty());
	}

	#[test]
	fn test_commit_produces_single_delta_key() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		queued.write("ns1", "", "key1", vec![1]).unwrap();
		queued.write("ns2", "", "key2", vec![2]).unwrap();
		queued.remove("ns1", "", "key3", true).unwrap();

		let committed = queued.commit().unwrap();
		assert_eq!(committed, 3);

		let delta_keys = inner
			.list(
				SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
				SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			.unwrap();
		assert_eq!(delta_keys.len(), 1);

		let ops = read_delta(&inner, &delta_keys[0]);
		assert_eq!(ops.len(), 3);
	}

	#[test]
	fn test_commit_sequence_numbers_increment() {
		let inner = TestStore::new();
		let queued = QueuedKVStoreSync::new(&inner);

		queued.write("ns", "", "key1", vec![1]).unwrap();
		queued.commit().unwrap();

		queued.write("ns", "", "key2", vec![2]).unwrap();
		queued.commit().unwrap();

		let delta_keys = inner
			.list(
				SYSTEM_DELTA_PERSISTENCE_PRIMARY_NAMESPACE,
				SYSTEM_DELTA_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			.unwrap();
		assert_eq!(delta_keys.len(), 2);
		assert!(delta_keys.contains(&"0".to_string()));
		assert!(delta_keys.contains(&"1".to_string()));
	}
}
