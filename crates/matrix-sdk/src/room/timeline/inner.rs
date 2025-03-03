// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
use eyeball_im::{ObservableVector, VectorSubscriber};
use im::Vector;
use indexmap::IndexSet;
use matrix_sdk_base::{
    crypto::OlmMachine,
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent, TimelineEvent},
    locks::{Mutex, MutexGuard},
};
use ruma::{
    events::{
        fully_read::FullyReadEvent, relation::Annotation, AnyMessageLikeEventContent,
        AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use tracing::{
    debug, error,
    field::{self, debug},
    info, info_span, warn, Instrument as _,
};
#[cfg(feature = "e2e-encryption")]
use tracing::{instrument, trace};

use super::{
    event_handler::{
        update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
        TimelineEventMetadata, TimelineItemPosition,
    },
    rfind_event_by_id, rfind_event_item, EventSendState, EventTimelineItem, InReplyToDetails,
    Message, Profile, RepliedToEvent, TimelineDetails, TimelineItem, TimelineItemContent,
};
use crate::{
    events::SyncTimelineEventWithoutContent,
    room::{self, timeline::event_item::RemoteEventTimelineItem},
    Error, Result,
};

#[derive(Debug)]
pub(super) struct TimelineInner<P: ProfileProvider = room::Common> {
    state: Mutex<TimelineInnerState>,
    profile_provider: P,
}

#[derive(Debug, Default)]
pub(super) struct TimelineInnerState {
    pub(super) items: ObservableVector<Arc<TimelineItem>>,
    /// Reaction event / txn ID => sender and reaction data.
    pub(super) reaction_map:
        HashMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), (OwnedUserId, Annotation)>,
    /// ID of event that is not in the timeline yet => List of reaction event
    /// IDs.
    pub(super) pending_reactions: HashMap<OwnedEventId, IndexSet<OwnedEventId>>,
    pub(super) fully_read_event: Option<OwnedEventId>,
    /// Whether the event that the fully-ready event _refers to_ is part of the
    /// timeline.
    pub(super) fully_read_event_in_timeline: bool,
}

impl<P: ProfileProvider> TimelineInner<P> {
    pub(super) fn new(profile_provider: P) -> Self {
        let state = TimelineInnerState {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            ..Default::default()
        };
        Self { state: Mutex::new(state), profile_provider }
    }

    /// Get a copy of the current items in the list.
    ///
    /// Cheap because `im::Vector` is cheap to clone.
    pub(super) async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.state.lock().await.items.clone()
    }

    pub(super) async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, VectorSubscriber<Arc<TimelineItem>>) {
        trace!("Creating timeline items signal");
        let state = self.state.lock().await;
        // auto-deref to the inner vector's clone method
        let items = state.items.clone();
        let stream = state.items.subscribe();
        (items, stream)
    }

    pub(super) async fn add_initial_events(&mut self, events: Vec<SyncTimelineEvent>) {
        if events.is_empty() {
            return;
        }

        debug!("Adding {} initial events", events.len());

        let state = self.state.get_mut();

        for event in events {
            handle_remote_event(
                event.event,
                event.encryption_info,
                TimelineItemPosition::End,
                state,
                &self.profile_provider,
            )
            .await;
        }
    }

    #[cfg(feature = "experimental-sliding-sync")]
    pub(super) async fn clear(&self) {
        trace!("Clearing timeline");

        let mut state = self.state.lock().await;
        state.items.clear();
        state.reaction_map.clear();
        state.fully_read_event = None;
        state.fully_read_event_in_timeline = false;
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_live_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
    ) {
        let mut state = self.state.lock().await;
        handle_remote_event(
            raw,
            encryption_info,
            TimelineItemPosition::End,
            &mut state,
            &self.profile_provider,
        )
        .await;
    }

    /// Handle the creation of a new local event.
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
    ) {
        let sender = self.profile_provider.own_user_id().to_owned();
        let sender_profile = self.profile_provider.profile(&sender).await;
        let event_meta = TimelineEventMetadata {
            sender,
            sender_profile,
            is_own_event: true,
            relations: Default::default(),
            // FIXME: Should we supply something here for encrypted rooms?
            encryption_info: None,
        };

        let flow = Flow::Local { txn_id, timestamp: MilliSecondsSinceUnixEpoch::now() };
        let kind = TimelineEventKind::Message { content };

        let mut state = self.state.lock().await;
        TimelineEventHandler::new(event_meta, flow, &mut state).handle_event(kind);
    }

    /// Update the send state of a local event represented by a transaction ID.
    ///
    /// If no local event is found, a warning is raised.
    #[instrument(skip_all, fields(txn_id))]
    pub(super) async fn update_event_send_state(
        &self,
        txn_id: &TransactionId,
        send_state: EventSendState,
    ) {
        let mut state = self.state.lock().await;

        let new_event_id: Option<&EventId> = match &send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        };

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&state.items, |it| {
            it.transaction_id() == Some(txn_id)
                || new_event_id.is_some() && it.event_id() == new_event_id
        });

        let Some((idx, item)) = result else {
            // Event isn't found at all.
            warn!("Timeline item not found, can't add event ID");
            return;
        };

        let EventTimelineItem::Local(item) = item else {
            // Remote echo already received. This is very unlikely.
            trace!("Remote echo received before send-event response");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &item.send_state {
            let new_event_id = new_event_id.map(debug);
            error!(?existing_event_id, ?new_event_id, "Local echo already marked as sent");
        }

        let new_item = TimelineItem::Event(item.with_send_state(send_state).into());
        state.items.set(idx, Arc::new(new_item));
    }

    /// Handle a back-paginated event.
    ///
    /// Returns the number of timeline updates that were made.
    #[instrument(skip_all)]
    pub(super) async fn handle_back_paginated_event(
        &self,
        event: TimelineEvent,
    ) -> HandleEventResult {
        let mut state = self.state.lock().await;
        handle_remote_event(
            event.event.cast(),
            event.encryption_info,
            TimelineItemPosition::Start,
            &mut state,
            &self.profile_provider,
        )
        .await
    }

    #[instrument(skip_all)]
    pub(super) async fn add_loading_indicator(&self) {
        let mut state = self.state.lock().await;

        if state.items.front().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is already a loading indicator");
            return;
        }

        state.items.push_front(Arc::new(TimelineItem::loading_indicator()));
    }

    #[instrument(skip(self))]
    pub(super) async fn remove_loading_indicator(&self, more_messages: bool) {
        let mut state = self.state.lock().await;

        if !state.items.front().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is no loading indicator");
            return;
        }

        if more_messages {
            state.items.pop_front();
        } else {
            state.items.set(0, Arc::new(TimelineItem::timeline_start()));
        }
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_fully_read(&self, raw: Raw<FullyReadEvent>) {
        let fully_read_event_id = match raw.deserialize() {
            Ok(ev) => ev.content.event_id,
            Err(e) => {
                error!("Failed to deserialize fully-read account data: {e}");
                return;
            }
        };

        self.set_fully_read_event(fully_read_event_id).await;
    }

    #[instrument(skip_all)]
    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        let mut state = self.state.lock().await;

        // A similar event has been handled already. We can ignore it.
        if state.fully_read_event.as_ref().map_or(false, |id| *id == fully_read_event_id) {
            return;
        }

        state.fully_read_event = Some(fully_read_event_id);

        let state = &mut *state;
        update_read_marker(
            &mut state.items,
            state.fully_read_event.as_deref(),
            &mut state.fully_read_event_in_timeline,
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, olm_machine))]
    pub(super) async fn retry_event_decryption(
        &self,
        room_id: &RoomId,
        olm_machine: &OlmMachine,
        session_ids: Option<BTreeSet<&str>>,
    ) {
        use super::EncryptedMessage;

        trace!("Retrying decryption");
        let should_retry = |session_id: &str| {
            if let Some(session_ids) = &session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        let retry_one = |item: Arc<TimelineItem>| {
            async move {
                let event_item = item.as_event()?;

                let session_id = match event_item.content().as_unable_to_decrypt()? {
                    EncryptedMessage::MegolmV1AesSha2 { session_id, .. }
                        if should_retry(session_id) =>
                    {
                        session_id
                    }
                    EncryptedMessage::MegolmV1AesSha2 { .. }
                    | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
                    | EncryptedMessage::Unknown => return None,
                };

                tracing::Span::current().record("session_id", session_id);

                let EventTimelineItem::Remote(
                    RemoteEventTimelineItem { event_id, raw, .. },
                ) = event_item else {
                    error!("Key for unable-to-decrypt timeline item is not an event ID");
                    return None;
                };

                tracing::Span::current().record("event_id", debug(event_id));

                let raw = raw.cast_ref();
                match olm_machine.decrypt_room_event(raw, room_id).await {
                    Ok(event) => {
                        trace!("Successfully decrypted event that previously failed to decrypt");
                        Some(event)
                    }
                    Err(e) => {
                        info!("Failed to decrypt event after receiving room key: {e}");
                        None
                    }
                }
            }
            .instrument(info_span!(
                "retry_one",
                session_id = field::Empty,
                event_id = field::Empty
            ))
        };

        let mut state = self.state.lock().await;

        // We loop through all the items in the timeline, if we successfully
        // decrypt a UTD item we either replace it or remove it and update
        // another one.
        let mut idx = 0;
        while let Some(item) = state.items.get(idx) {
            let Some(event) = retry_one(item.clone()).await else {
                idx += 1;
                continue;
            };

            let result = handle_remote_event(
                event.event.cast(),
                event.encryption_info,
                TimelineItemPosition::Update(idx),
                &mut state,
                &self.profile_provider,
            )
            .await;

            // If the UTD was removed rather than updated, run the loop again
            // with the same index.
            if !result.item_removed {
                idx += 1;
            }
        }
    }

    pub(super) async fn set_sender_profiles_pending(&self) {
        self.set_non_ready_sender_profiles(TimelineDetails::Pending).await;
    }

    pub(super) async fn set_sender_profiles_error(&self, error: Arc<Error>) {
        self.set_non_ready_sender_profiles(TimelineDetails::Error(error)).await;
    }

    async fn set_non_ready_sender_profiles(&self, profile_state: TimelineDetails<Profile>) {
        let mut state = self.state.lock().await;
        for idx in 0..state.items.len() {
            let Some(event_item) = state.items[idx].as_event() else { continue };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                let item = Arc::new(TimelineItem::Event(
                    event_item.with_sender_profile(profile_state.clone()),
                ));
                state.items.set(idx, item);
            }
        }
    }

    pub(super) async fn update_sender_profiles(&self) {
        trace!("Updating sender profiles");

        let mut state = self.state.lock().await;
        let num_items = state.items.len();

        for idx in 0..num_items {
            let sender = match state.items[idx].as_event() {
                Some(event_item) => event_item.sender().to_owned(),
                None => continue,
            };
            let maybe_profile = self.profile_provider.profile(&sender).await;

            assert_eq!(state.items.len(), num_items);

            let event_item = state.items[idx].as_event().unwrap();
            match maybe_profile {
                Some(profile) => {
                    if !event_item.sender_profile().contains(&profile) {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        state.items.set(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        state.items.set(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
            }
        }
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &room::Common {
        &self.profile_provider
    }

    pub(super) async fn fetch_in_reply_to_details(
        &self,
        event_id: &EventId,
    ) -> Result<RemoteEventTimelineItem> {
        let state = self.state.lock().await;
        let (index, item) = rfind_event_by_id(&state.items, event_id)
            .and_then(|(pos, item)| item.as_remote().map(|item| (pos, item.clone())))
            .ok_or(super::Error::RemoteEventNotInTimeline)?;

        let TimelineItemContent::Message(message) = item.content.clone() else {
            return Ok(item);
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            return Ok(item);
        };

        let details = fetch_replied_to_event(
            state,
            index,
            &item,
            &message,
            &in_reply_to.event_id,
            self.room(),
        )
        .await;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let mut state = self.state.lock().await;
        let (index, mut item) = rfind_event_by_id(&state.items, &item.event_id)
            .and_then(|(pos, item)| item.as_remote().map(|item| (pos, item.clone())))
            .ok_or(super::Error::RemoteEventNotInTimeline)?;

        // Check the state of the event again, it might have been redacted while
        // the request was in-flight.
        let TimelineItemContent::Message(message) = item.content.clone() else {
            return Ok(item);
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            return Ok(item);
        };

        item.content = TimelineItemContent::Message(message.with_in_reply_to(InReplyToDetails {
            event_id: in_reply_to.event_id.clone(),
            details,
        }));
        state.items.set(index, Arc::new(TimelineItem::Event(item.clone().into())));

        Ok(item)
    }
}

async fn fetch_replied_to_event(
    mut state: MutexGuard<'_, TimelineInnerState>,
    index: usize,
    item: &RemoteEventTimelineItem,
    message: &Message,
    in_reply_to: &EventId,
    room: &room::Common,
) -> TimelineDetails<Box<RepliedToEvent>> {
    if let Some((_, item)) = rfind_event_by_id(&state.items, in_reply_to) {
        let details = match item.content() {
            TimelineItemContent::Message(message) => {
                TimelineDetails::Ready(Box::new(RepliedToEvent {
                    message: message.clone(),
                    sender: item.sender().to_owned(),
                    sender_profile: item.sender_profile().clone(),
                }))
            }
            _ => TimelineDetails::Error(Arc::new(super::Error::UnsupportedEvent.into())),
        };

        return details;
    };

    let event_item = item
        .with_content(TimelineItemContent::Message(message.with_in_reply_to(InReplyToDetails {
            event_id: in_reply_to.to_owned(),
            details: TimelineDetails::Pending,
        })))
        .into();
    state.items.set(index, Arc::new(TimelineItem::Event(event_item)));

    // Don't hold the state lock while the network request is made
    drop(state);

    match room.event(in_reply_to).await {
        Ok(timeline_event) => {
            match RepliedToEvent::try_from_timeline_event(timeline_event, room).await {
                Ok(event) => TimelineDetails::Ready(Box::new(event)),
                Err(e) => TimelineDetails::Error(Arc::new(e)),
            }
        }
        Err(e) => TimelineDetails::Error(Arc::new(e)),
    }
}

#[async_trait]
pub(super) trait ProfileProvider {
    fn own_user_id(&self) -> &UserId;
    async fn profile(&self, user_id: &UserId) -> Option<Profile>;
}

#[async_trait]
impl ProfileProvider for room::Common {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    async fn profile(&self, user_id: &UserId) -> Option<Profile> {
        match self.get_member_no_sync(user_id).await {
            Ok(Some(member)) => Some(Profile {
                display_name: member.display_name().map(ToOwned::to_owned),
                display_name_ambiguous: member.name_ambiguous(),
                avatar_url: member.avatar_url().map(ToOwned::to_owned),
            }),
            Ok(None) if self.are_members_synced() => Some(Profile {
                display_name: None,
                display_name_ambiguous: false,
                avatar_url: None,
            }),
            Ok(None) => None,
            Err(e) => {
                error!(%user_id, "Failed to getch room member information: {e}");
                None
            }
        }
    }
}

/// Handle a remote event.
///
/// Returns the number of timeline updates that were made.
async fn handle_remote_event<P: ProfileProvider>(
    raw: Raw<AnySyncTimelineEvent>,
    encryption_info: Option<EncryptionInfo>,
    position: TimelineItemPosition,
    timeline_state: &mut TimelineInnerState,
    profile_provider: &P,
) -> HandleEventResult {
    let (event_id, sender, origin_server_ts, txn_id, relations, event_kind) =
        match raw.deserialize() {
            Ok(event) => (
                event.event_id().to_owned(),
                event.sender().to_owned(),
                event.origin_server_ts(),
                event.transaction_id().map(ToOwned::to_owned),
                event.relations().to_owned(),
                event.into(),
            ),
            Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
                Ok(event) => (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    event.relations().to_owned(),
                    TimelineEventKind::failed_to_parse(event, e),
                ),
                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(event_type, event_id, "Failed to deserialize timeline event: {e}");
                    return HandleEventResult::default();
                }
            },
        };

    let is_own_event = sender == profile_provider.own_user_id();
    let sender_profile = profile_provider.profile(&sender).await;
    let event_meta =
        TimelineEventMetadata { sender, sender_profile, is_own_event, relations, encryption_info };
    let flow = Flow::Remote { event_id, origin_server_ts, raw_event: raw, txn_id, position };

    TimelineEventHandler::new(event_meta, flow, timeline_state).handle_event(event_kind)
}
