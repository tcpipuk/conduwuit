use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ruma::{events::StateEventType, EventId, RoomId};

use crate::{services, utils, Error, KeyValueDatabase, PduEvent, Result};

#[async_trait]
pub trait Data: Send + Sync {
	/// Builds a StateMap by iterating over all keys that start
	/// with state_hash, this gives the full state for the given state_hash.
	#[allow(unused_qualifications)] // async traits
	async fn state_full_ids(&self, shortstatehash: u64) -> Result<HashMap<u64, Arc<EventId>>>;

	#[allow(unused_qualifications)] // async traits
	async fn state_full(&self, shortstatehash: u64) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>>;

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn state_get_id(
		&self, shortstatehash: u64, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<EventId>>>;

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn state_get(
		&self, shortstatehash: u64, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<PduEvent>>>;

	/// Returns the state hash for this pdu.
	fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>>;

	/// Returns the full room state.
	#[allow(unused_qualifications)] // async traits
	async fn room_state_full(&self, room_id: &RoomId) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>>;

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn room_state_get_id(
		&self, room_id: &RoomId, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<EventId>>>;

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn room_state_get(
		&self, room_id: &RoomId, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<PduEvent>>>;
}

#[async_trait]
impl Data for KeyValueDatabase {
	#[allow(unused_qualifications)] // async traits
	async fn state_full_ids(&self, shortstatehash: u64) -> Result<HashMap<u64, Arc<EventId>>> {
		let full_state = services()
			.rooms
			.state_compressor
			.load_shortstatehash_info(shortstatehash)?
			.pop()
			.expect("there is always one layer")
			.1;
		let mut result = HashMap::new();
		let mut i: u8 = 0;
		for compressed in full_state.iter() {
			let parsed = services()
				.rooms
				.state_compressor
				.parse_compressed_state_event(compressed)?;
			result.insert(parsed.0, parsed.1);

			i = i.wrapping_add(1);
			if i % 100 == 0 {
				tokio::task::yield_now().await;
			}
		}
		Ok(result)
	}

	#[allow(unused_qualifications)] // async traits
	async fn state_full(&self, shortstatehash: u64) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
		let full_state = services()
			.rooms
			.state_compressor
			.load_shortstatehash_info(shortstatehash)?
			.pop()
			.expect("there is always one layer")
			.1;

		let mut result = HashMap::new();
		let mut i: u8 = 0;
		for compressed in full_state.iter() {
			let (_, eventid) = services()
				.rooms
				.state_compressor
				.parse_compressed_state_event(compressed)?;
			if let Some(pdu) = services().rooms.timeline.get_pdu(&eventid)? {
				result.insert(
					(
						pdu.kind.to_string().into(),
						pdu.state_key
							.as_ref()
							.ok_or_else(|| Error::bad_database("State event has no state key."))?
							.clone(),
					),
					pdu,
				);
			}

			i = i.wrapping_add(1);
			if i % 100 == 0 {
				tokio::task::yield_now().await;
			}
		}

		Ok(result)
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn state_get_id(
		&self, shortstatehash: u64, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<EventId>>> {
		let Some(shortstatekey) = services()
			.rooms
			.short
			.get_shortstatekey(event_type, state_key)?
		else {
			return Ok(None);
		};
		let full_state = services()
			.rooms
			.state_compressor
			.load_shortstatehash_info(shortstatehash)?
			.pop()
			.expect("there is always one layer")
			.1;
		Ok(full_state
			.iter()
			.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
			.and_then(|compressed| {
				services()
					.rooms
					.state_compressor
					.parse_compressed_state_event(compressed)
					.ok()
					.map(|(_, id)| id)
			}))
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn state_get(
		&self, shortstatehash: u64, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<PduEvent>>> {
		self.state_get_id(shortstatehash, event_type, state_key)?
			.map_or(Ok(None), |event_id| services().rooms.timeline.get_pdu(&event_id))
	}

	/// Returns the state hash for this pdu.
	fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
		self.eventid_shorteventid
			.get(event_id.as_bytes())?
			.map_or(Ok(None), |shorteventid| {
				self.shorteventid_shortstatehash
					.get(&shorteventid)?
					.map(|bytes| {
						utils::u64_from_bytes(&bytes).map_err(|_| {
							Error::bad_database("Invalid shortstatehash bytes in shorteventid_shortstatehash")
						})
					})
					.transpose()
			})
	}

	/// Returns the full room state.
	#[allow(unused_qualifications)] // async traits
	async fn room_state_full(&self, room_id: &RoomId) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
		if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
			self.state_full(current_shortstatehash).await
		} else {
			Ok(HashMap::new())
		}
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn room_state_get_id(
		&self, room_id: &RoomId, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<EventId>>> {
		if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
			self.state_get_id(current_shortstatehash, event_type, state_key)
		} else {
			Ok(None)
		}
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	fn room_state_get(
		&self, room_id: &RoomId, event_type: &StateEventType, state_key: &str,
	) -> Result<Option<Arc<PduEvent>>> {
		if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
			self.state_get(current_shortstatehash, event_type, state_key)
		} else {
			Ok(None)
		}
	}
}
