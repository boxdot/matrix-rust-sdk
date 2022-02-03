// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! The crypto specific Olm objects.
//!
//! Note: You'll only be interested in these if you are implementing a custom
//! `CryptoStore`.

mod account;
mod group_sessions;
mod session;
mod signing;
mod utility;

pub(crate) use account::{Account, OlmDecryptionInfo, SessionType};
pub use account::{AccountPickle, OlmMessageHash, PickledAccount, ReadOnlyAccount};
pub use group_sessions::{
    EncryptionSettings, ExportedRoomKey, InboundGroupSession, InboundGroupSessionPickle,
    OutboundGroupSession, PickledInboundGroupSession, PickledOutboundGroupSession, ShareInfo,
};
pub(crate) use group_sessions::{GroupSessionKey, ShareState};
use matrix_sdk_common::instant::{Duration, Instant};
pub use olm_rs::{account::IdentityKeys, PicklingMode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub use session::{PickledSession, Session, SessionPickle};
pub use signing::{CrossSigningStatus, PickledCrossSigningIdentity, PrivateCrossSigningIdentity};
pub(crate) use utility::Utility;

pub(crate) fn serialize_instant<S>(instant: &Instant, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let duration = instant.elapsed();
    duration.serialize(serializer)
}

pub(crate) fn deserialize_instant<'de, D>(deserializer: D) -> Result<Instant, D::Error>
where
    D: Deserializer<'de>,
{
    let duration = Duration::deserialize(deserializer)?;
    let now = Instant::now();
    let instant = now
        .checked_sub(duration)
        .ok_or_else(|| serde::de::Error::custom("Can't subtract the current instant"))?;
    Ok(instant)
}

#[cfg(test)]
pub(crate) mod test {

    use std::{collections::BTreeMap, convert::TryInto};

    use matches::assert_matches;
    use matrix_sdk_test::async_test;
    use olm_rs::session::OlmMessage;
    use ruma::{
        device_id,
        encryption::SignedKey,
        event_id,
        events::{
            forwarded_room_key::ToDeviceForwardedRoomKeyEventContent,
            room::message::{Relation, Replacement, RoomMessageEventContent},
            AnyMessageEvent, AnyRoomEvent, AnySyncMessageEvent, AnySyncRoomEvent,
        },
        room_id,
        serde::Base64,
        user_id, DeviceId, UserId,
    };
    use serde_json::json;

    use crate::olm::{InboundGroupSession, ReadOnlyAccount, Session};

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("ALICEDEVICE")
    }

    fn bob_id() -> &'static UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> &'static DeviceId {
        device_id!("BOBDEVICE")
    }

    pub(crate) async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key =
            Base64::parse(bob.one_time_keys().await.curve25519().values().next().unwrap()).unwrap();
        let one_time_key = SignedKey::new(one_time_key, BTreeMap::new());
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session =
            alice.create_outbound_session_helper(&sender_key, &one_time_key).await.unwrap();

        (alice, session)
    }

    #[test]
    fn account_creation() {
        let account = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let identity_keys = account.identity_keys();

        assert!(!account.shared());
        assert!(!identity_keys.ed25519().is_empty());
        assert_ne!(identity_keys.values().len(), 0);
        assert_ne!(identity_keys.keys().len(), 0);
        assert_ne!(identity_keys.iter().len(), 0);
        assert!(identity_keys.contains_key("ed25519"));
        assert_eq!(identity_keys.ed25519(), identity_keys.get("ed25519").unwrap());
        assert!(!identity_keys.curve25519().is_empty());

        account.mark_as_shared();
        assert!(account.shared());
    }

    #[async_test]
    async fn one_time_keys_creation() {
        let account = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let one_time_keys = account.one_time_keys().await;

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys().await, 0);

        account.generate_one_time_keys_helper(10).await;
        let one_time_keys = account.one_time_keys().await;

        assert!(!one_time_keys.curve25519().is_empty());
        assert_ne!(one_time_keys.values().len(), 0);
        assert_ne!(one_time_keys.keys().len(), 0);
        assert_ne!(one_time_keys.iter().len(), 0);
        assert!(one_time_keys.contains_key("curve25519"));
        assert_eq!(one_time_keys.curve25519().keys().len(), 10);
        assert_eq!(one_time_keys.curve25519(), one_time_keys.get("curve25519").unwrap());

        account.mark_keys_as_published().await;
        let one_time_keys = account.one_time_keys().await;
        assert!(one_time_keys.curve25519().is_empty());
    }

    #[async_test]
    async fn session_creation() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let alice_keys = alice.identity_keys();
        alice.generate_one_time_keys_helper(1).await;
        let one_time_keys = alice.one_time_keys().await;
        alice.mark_keys_as_published().await;

        let one_time_key =
            Base64::parse(one_time_keys.curve25519().values().next().unwrap()).unwrap();

        let one_time_key = SignedKey::new(one_time_key, BTreeMap::new());

        let mut bob_session = bob
            .create_outbound_session_helper(alice_keys.curve25519(), &one_time_key)
            .await
            .unwrap();

        let plaintext = "Hello world";

        let message = bob_session.encrypt_helper(plaintext).await;

        let prekey_message = match message.clone() {
            OlmMessage::PreKey(m) => m,
            OlmMessage::Message(_) => panic!("Incorrect message type"),
        };

        let bob_keys = bob.identity_keys();
        let mut alice_session = alice
            .create_inbound_session(bob_keys.curve25519(), prekey_message.clone())
            .await
            .unwrap();

        assert!(alice_session.matches(bob_keys.curve25519(), prekey_message).await.unwrap());

        assert_eq!(bob_session.session_id(), alice_session.session_id());

        let decyrpted = alice_session.decrypt(message).await.unwrap();
        assert_eq!(plaintext, decyrpted);
    }

    #[async_test]
    async fn group_session_creation() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (outbound, _) = alice.create_group_session_pair_with_defaults(room_id).await.unwrap();

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            room_id,
            outbound.session_key().await,
            None,
        )
        .unwrap();

        assert_eq!(0, inbound.first_known_index());

        assert_eq!(outbound.session_id(), inbound.session_id());

        let plaintext = "This is a secret to everybody".to_owned();
        let ciphertext = outbound.encrypt_helper(plaintext.clone()).await;

        assert_eq!(plaintext, inbound.decrypt_helper(ciphertext).await.unwrap().0);
    }

    #[async_test]
    async fn edit_decryption() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");
        let event_id = event_id!("$1234adfad:asdf");

        let (outbound, _) = alice.create_group_session_pair_with_defaults(room_id).await.unwrap();

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let mut content = RoomMessageEventContent::text_plain("Hello");
        content.relates_to = Some(Relation::Replacement(Replacement::new(
            event_id.to_owned(),
            RoomMessageEventContent::text_plain("Hello edit").into(),
        )));

        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            room_id,
            outbound.session_key().await,
            None,
        )
        .unwrap();

        assert_eq!(0, inbound.first_known_index());

        assert_eq!(outbound.session_id(), inbound.session_id());

        let encrypted_content =
            outbound.encrypt(serde_json::to_value(content).unwrap(), "m.room.message").await;

        let event = json!({
            "sender": alice.user_id(),
            "event_id": event_id,
            "origin_server_ts": 0u64,
            "room_id": room_id,
            "type": "m.room.encrypted",
            "content": encrypted_content,
        })
        .to_string();

        let event: AnySyncRoomEvent = serde_json::from_str(&event).unwrap();

        let event =
            if let AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomEncrypted(event)) = event {
                event
            } else {
                panic!("Invalid event type")
            };

        let decrypted = inbound.decrypt(&event).await.unwrap().0;

        if let AnyRoomEvent::Message(AnyMessageEvent::RoomMessage(e)) =
            decrypted.deserialize().unwrap()
        {
            assert_matches!(e.content.relates_to, Some(Relation::Replacement(_)));
        } else {
            panic!("Invalid event type")
        }
    }

    #[async_test]
    async fn group_session_export() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, inbound) = alice.create_group_session_pair_with_defaults(room_id).await.unwrap();

        let export = inbound.export().await;
        let export: ToDeviceForwardedRoomKeyEventContent = export.try_into().unwrap();

        let imported = InboundGroupSession::from_export(export).unwrap();

        assert_eq!(inbound.session_id(), imported.session_id());
    }
}
