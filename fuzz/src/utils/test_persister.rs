use lightning::chain;
use lightning::chain::{chainmonitor, channelmonitor};
use lightning::ln::types::ChannelId;
use lightning::util::hash_tables::*;
use lightning::util::persist::MonitorName;
use lightning::util::ser::Writeable;
use lightning::util::test_channel_signer::TestChannelSigner;

use std::sync::Mutex;

struct VecWriter(pub Vec<u8>);
impl lightning::util::ser::Writer for VecWriter {
	fn write_all(&mut self, buf: &[u8]) -> Result<(), lightning::io::Error> {
		self.0.extend_from_slice(buf);
		Ok(())
	}
}

/// The LDK API requires that any time we tell it we're done persisting a `ChannelMonitor[Update]`
/// we never pass it in as the "latest" `ChannelMonitor` on startup. However, we can pass
/// out-of-date monitors as long as we never told LDK we finished persisting them, which we do by
/// storing both old `ChannelMonitor`s and ones that are "being persisted" here.
///
/// Note that such "being persisted" `ChannelMonitor`s are stored in `ChannelManager` and will
/// simply be replayed on startup.
pub struct LatestMonitorState {
	/// The latest monitor id which we told LDK we've persisted.
	///
	/// Note that there may still be earlier pending monitor updates in [`Self::pending_monitors`]
	/// which we haven't yet completed. We're allowed to reload with those as well, at least until
	/// they're completed.
	pub persisted_monitor_id: u64,
	/// The latest serialized `ChannelMonitor` that we told LDK we persisted.
	pub persisted_monitor: Vec<u8>,
	/// A set of (monitor id, serialized `ChannelMonitor`)s which we're currently "persisting",
	/// from LDK's perspective.
	pub pending_monitors: Vec<(u64, Vec<u8>)>,
}

pub struct TestPersister {
	pub update_ret: Mutex<chain::ChannelMonitorUpdateStatus>,
	pub latest_monitors: Mutex<HashMap<ChannelId, LatestMonitorState>>,
}

impl TestPersister {
	pub fn new(update_ret: chain::ChannelMonitorUpdateStatus) -> Self {
		Self { update_ret: Mutex::new(update_ret), latest_monitors: Mutex::new(new_hash_map()) }
	}

	/// Returns all pending monitor updates for all channels, clearing them from the pending state.
	/// Returns a Vec of (channel_id, update_id) pairs that should be passed to
	/// `ChainMonitor::channel_monitor_updated`.
	pub fn complete_all_pending_monitor_updates(&self) -> Vec<(ChannelId, u64)> {
		let mut completed = Vec::new();
		for (channel_id, state) in self.latest_monitors.lock().unwrap().iter_mut() {
			for (id, data) in state.pending_monitors.drain(..) {
				completed.push((*channel_id, id));
				if id >= state.persisted_monitor_id {
					state.persisted_monitor_id = id;
					state.persisted_monitor = data;
				}
			}
		}
		completed
	}

	/// Returns a single pending monitor update for the given channel, selected by the provided
	/// selector. Returns Some((channel_id, update_id)) if an update was selected, or None.
	pub fn complete_monitor_update(
		&self, chan_id: &ChannelId,
		compl_selector: &dyn Fn(&mut Vec<(u64, Vec<u8>)>) -> Option<(u64, Vec<u8>)>,
	) -> Option<(ChannelId, u64)> {
		if let Some(state) = self.latest_monitors.lock().unwrap().get_mut(chan_id) {
			assert!(
				state.pending_monitors.windows(2).all(|pair| pair[0].0 < pair[1].0),
				"updates should be sorted by id"
			);
			if let Some((id, data)) = compl_selector(&mut state.pending_monitors) {
				if id > state.persisted_monitor_id {
					state.persisted_monitor_id = id;
					state.persisted_monitor = data;
				}
				return Some((*chan_id, id));
			}
		}
		None
	}

	/// Returns all pending monitor updates for a specific channel, clearing them from the pending
	/// state. Returns a Vec of (channel_id, update_id) pairs.
	pub fn complete_all_monitor_updates(&self, chan_id: &ChannelId) -> Vec<(ChannelId, u64)> {
		let mut completed = Vec::new();
		if let Some(state) = self.latest_monitors.lock().unwrap().get_mut(chan_id) {
			assert!(
				state.pending_monitors.windows(2).all(|pair| pair[0].0 < pair[1].0),
				"updates should be sorted by id"
			);
			for (id, data) in state.pending_monitors.drain(..) {
				completed.push((*chan_id, id));
				if id > state.persisted_monitor_id {
					state.persisted_monitor_id = id;
					state.persisted_monitor = data;
				}
			}
		}
		completed
	}
}

impl chainmonitor::Persist<TestChannelSigner> for TestPersister {
	fn persist_new_channel(
		&self, _monitor_name: MonitorName, data: &channelmonitor::ChannelMonitor<TestChannelSigner>,
	) -> chain::ChannelMonitorUpdateStatus {
		let mut ser = VecWriter(Vec::new());
		data.write(&mut ser).unwrap();
		let monitor_id = data.get_latest_update_id();
		let res = self.update_ret.lock().unwrap().clone();

		let state = match res {
			chain::ChannelMonitorUpdateStatus::Completed => LatestMonitorState {
				persisted_monitor_id: monitor_id,
				persisted_monitor: ser.0,
				pending_monitors: Vec::new(),
			},
			chain::ChannelMonitorUpdateStatus::InProgress => LatestMonitorState {
				persisted_monitor_id: monitor_id,
				persisted_monitor: Vec::new(),
				pending_monitors: vec![(monitor_id, ser.0)],
			},
			chain::ChannelMonitorUpdateStatus::UnrecoverableError => panic!(),
		};

		let channel_id = data.channel_id();
		if self.latest_monitors.lock().unwrap().insert(channel_id, state).is_some() {
			panic!("Already had monitor pre-persist_new_channel");
		}
		res
	}

	fn update_persisted_channel(
		&self, _monitor_name: MonitorName, update: Option<&channelmonitor::ChannelMonitorUpdate>,
		data: &channelmonitor::ChannelMonitor<TestChannelSigner>,
	) -> chain::ChannelMonitorUpdateStatus {
		let mut ser = VecWriter(Vec::new());
		data.write(&mut ser).unwrap();
		let res = self.update_ret.lock().unwrap().clone();

		let channel_id = data.channel_id();
		let mut map_lock = self.latest_monitors.lock().unwrap();
		let map_entry = map_lock.get_mut(&channel_id).expect("Didn't have monitor on update call");

		match res {
			chain::ChannelMonitorUpdateStatus::Completed => {
				if let Some(update) = update {
					map_entry.persisted_monitor_id = update.update_id;
				}
				map_entry.persisted_monitor = ser.0;
			},
			chain::ChannelMonitorUpdateStatus::InProgress => {
				if let Some(update) = update {
					map_entry.pending_monitors.push((update.update_id, ser.0));
				}
			},
			chain::ChannelMonitorUpdateStatus::UnrecoverableError => panic!(),
		}
		res
	}

	fn archive_persisted_channel(&self, _monitor_name: MonitorName) {}
}
