use dashmap::DashMap;
#[cfg(feature = "simd-json")]
use halfbrown::hashmap;
use serde::Serialize;
#[cfg(not(feature = "simd-json"))]
use serde_json::{to_string, Value as OwnedValue};
#[cfg(feature = "simd-json")]
use simd_json::{to_string, OwnedValue};
use twilight_cache_inmemory::{InMemoryCache, InMemoryCacheStats};
use twilight_model::{
    channel::{message::Sticker, Channel, StageInstance},
    gateway::{
        payload::incoming::{GuildDelete, VoiceServerUpdate},
        presence::{Presence, UserOrId},
        OpCode,
    },
    guild::{scheduled_event::GuildScheduledEvent, Emoji, Guild, Member, Role},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
    voice::VoiceState,
};

use std::sync::Arc;

use crate::model::JsonObject;

#[derive(Serialize)]
pub struct Payload<T> {
    pub d: T,
    pub op: OpCode,
    pub t: &'static str,
    pub s: usize,
}

pub struct Cache {
    // This is the global cache
    inner: Arc<InMemoryCache>,
    // This can be shard-local since it should only concern the guilds on this shard
    voice_servers: DashMap<Id<GuildMarker>, VoiceServerUpdate>,
}

impl Cache {
    pub fn new(cache: Arc<InMemoryCache>) -> Self {
        Self {
            inner: cache,
            voice_servers: DashMap::new(),
        }
    }

    pub fn update(&self, value: twilight_gateway::Event) {
        match value {
            // The generic cache doesn't cache VoiceServerUpdates - it is a terrible idea
            // but we have to do it, in order to to reuse voice connections.
            twilight_gateway::Event::VoiceServerUpdate(ref voice_server) => {
                if voice_server.token.is_empty() || voice_server.endpoint.as_ref().map(|e| e.is_empty()).unwrap_or(true) {
                    tracing::warn!(
                        ?voice_server,
                        "Received invalid VoiceServerUpdate (missing token or endpoint); removing cache entry"
                    );
                    self.voice_servers.remove(&voice_server.guild_id);
                } else {
                    self.voice_servers
                        .insert(voice_server.guild_id, voice_server.clone());
                }
            }
            // If we are disconnecting from a voice channel or changing channel, delete the voice server cache
            twilight_gateway::Event::VoiceStateUpdate(ref voice_state) => {
                if let Some(guild_id) = voice_state.guild_id {
                    if let Some(user_id) = self.inner.current_user().map(|u| u.id) {
                        if voice_state.user_id == user_id {
                            // Disconnected
                            if voice_state.channel_id.is_none() {
                                self.voice_servers.remove(&guild_id);
                            }
                            if let Some(cached) = self.inner.voice_state(user_id, guild_id) {
                                // Moved to another channel
                                if voice_state
                                    .channel_id
                                    .map_or(true, |channel_id| channel_id != cached.channel_id())
                                {
                                    self.voice_servers.remove(&guild_id);
                                }
                            }
                        }
                    }
                }
            }
            // If we are being kicked from a guild, delete the voice server cache and voice stats
            twilight_gateway::Event::GuildDelete(ref guild) => {
                if guild.unavailable != Some(true) {
                    self.voice_servers.remove(&guild.id);
                }
            }
            _ => {}
        }

        // Update the generic cache
        self.inner.update(value);
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn stats(&self) -> InMemoryCacheStats<'_> {
        self.inner.stats()
    }

    pub fn my_voice_state(&self, guild_id: Id<GuildMarker>) -> Option<VoiceState> {
        if let Some(user) = self.inner.current_user() {
            if let Some(voice_state) = self.inner.voice_state(user.id, guild_id) {
                let state = VoiceState {
                    channel_id: Some(voice_state.channel_id()),
                    deaf: voice_state.deaf(),
                    guild_id: Some(voice_state.guild_id()),
                    member: self.member(guild_id, user.id),
                    mute: voice_state.mute(),
                    self_deaf: voice_state.self_deaf(),
                    self_mute: voice_state.self_mute(),
                    self_stream: voice_state.self_stream(),
                    self_video: voice_state.self_video(),
                    session_id: voice_state.session_id().to_string(),
                    suppress: voice_state.suppress(),
                    user_id: voice_state.user_id(),
                    request_to_speak_timestamp: voice_state.request_to_speak_timestamp(),
                };

                return Some(state);
            }
        }

        None
    }

    pub fn get_voice_server(&self, guild_id: Id<GuildMarker>) -> Option<VoiceServerUpdate> {
        self.voice_servers.get(&guild_id).map(|r| r.clone())
    }

    pub fn get_voice_state_update_response(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Option<(Payload<Box<VoiceState>>, Payload<VoiceServerUpdate>)> {
        // If the client is connecting to a channel it is already connected to, return cached info
        if let Some(user_id) = self.inner.current_user().map(|u| u.id) {
            if let Some(cached) = self.inner.voice_state(user_id, guild_id) {
                if channel_id == cached.channel_id() {
                    if let (Some(voice_state), Some(voice_server)) = (
                        self.my_voice_state(guild_id),
                        self.get_voice_server(guild_id),
                    ) {
                        let voice_state_update = Payload {
                            d: Box::new(voice_state),
                            op: OpCode::Dispatch,
                            t: "VOICE_STATE_UPDATE",
                            s: 0,
                        };

                        let voice_server_update = Payload {
                            d: voice_server,
                            op: OpCode::Dispatch,
                            t: "VOICE_SERVER_UPDATE",
                            s: 0,
                        };

                        return Some((voice_state_update, voice_server_update));
                    }
                }
            }
        }

        None
    }

    pub fn get_ready_payload(
        &self,
        mut ready: JsonObject,
        sequence: &mut usize,
    ) -> Payload<JsonObject> {
        *sequence += 1;

        let guild_id_to_json = |guild_id: Id<GuildMarker>| {
            #[cfg(feature = "simd-json")]
            {
                OwnedValue::Object(Box::new(hashmap! {
                    String::from("id") => guild_id.to_string().into(),
                    String::from("unavailable") => true.into(),
                }))
            }
            #[cfg(not(feature = "simd-json"))]
            {
                serde_json::json!({
                    "id": guild_id.to_string(),
                    "unavailable": true
                })
            }
        };

        let guilds: Vec<_> = self
            .inner
            .iter()
            .guilds()
            .filter_map(|guild| {
                if guild.unavailable() == Some(true) {
                    // Will be part of unavailable_guilds iterator
                    None
                } else {
                    Some(guild_id_to_json(guild.id()))
                }
            })
            .chain(self.inner.iter().unavailable_guilds().map(guild_id_to_json))
            .collect();

        ready.insert(String::from("guilds"), OwnedValue::Array(guilds.into()));

        Payload {
            d: ready,
            op: OpCode::Dispatch,
            t: "READY",
            s: *sequence,
        }
    }

    fn channels_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Channel> {
        self.inner
            .guild_channels(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|channel_id| {
                        let channel = self.inner.channel(*channel_id)?;

                        if channel.kind.is_thread() {
                            None
                        } else {
                            Some(channel.value().clone())
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn presences_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Presence> {
        self.inner
            .guild_presences(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|user_id| {
                        let presence = self.inner.presence(guild_id, *user_id)?;

                        Some(Presence {
                            activities: presence.activities().to_vec(),
                            client_status: presence.client_status().clone(),
                            guild_id: presence.guild_id(),
                            status: presence.status(),
                            user: UserOrId::UserId {
                                id: presence.user_id(),
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn emojis_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Emoji> {
        self.inner
            .guild_emojis(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|emoji_id| {
                        let emoji = self.inner.emoji(*emoji_id)?;

                        Some(Emoji {
                            animated: emoji.animated(),
                            available: emoji.available(),
                            id: emoji.id(),
                            managed: emoji.managed(),
                            name: emoji.name().to_string(),
                            require_colons: emoji.require_colons(),
                            roles: emoji.roles().to_vec(),
                            user: emoji
                                .user_id()
                                .and_then(|id| self.inner.user(id).map(|user| user.value().clone())),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn member(&self, guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) -> Option<Member> {
        let member = self.inner.member(guild_id, user_id)?;

        Some(Member {
            avatar: member.avatar(),
            communication_disabled_until: member.communication_disabled_until(),
            deaf: member.deaf().unwrap_or_default(),
            flags: member.flags(),
            joined_at: member.joined_at(),
            mute: member.mute().unwrap_or_default(),
            nick: member.nick().map(ToString::to_string),
            pending: member.pending(),
            premium_since: member.premium_since(),
            roles: member.roles().to_vec(),
            user: self.inner.user(member.user_id())?.value().clone(),
        })
    }

    fn members_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Member> {
        self.inner
            .guild_members(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|user_id| self.member(guild_id, *user_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn roles_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Role> {
        self.inner
            .guild_roles(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|role_id| Some(self.inner.role(*role_id)?.value().resource().clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn scheduled_events_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<GuildScheduledEvent> {
        self.inner
            .scheduled_events(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|event_id| {
                        Some(
                            self.inner
                                .scheduled_event(*event_id)?
                                .value()
                                .resource()
                                .clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stage_instances_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<StageInstance> {
        self.inner
            .guild_stage_instances(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|stage_id| {
                        Some(self.inner.stage_instance(*stage_id)?.value().resource().clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stickers_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Sticker> {
        self.inner
            .guild_stickers(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|sticker_id| {
                        let sticker = self.inner.sticker(*sticker_id)?;

                        Some(Sticker {
                            available: sticker.available(),
                            description: Some(sticker.description().to_string()),
                            format_type: sticker.format_type(),
                            guild_id: Some(sticker.guild_id()),
                            id: sticker.id(),
                            kind: sticker.kind(),
                            name: sticker.name().to_string(),
                            pack_id: sticker.pack_id(),
                            sort_value: sticker.sort_value(),
                            tags: sticker.tags().to_string(),
                            user: sticker
                                .user_id()
                                .and_then(|id| self.inner.user(id).map(|user| user.value().clone())),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn voice_states_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<VoiceState> {
        self.inner
            .guild_voice_states(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|user_id| {
                        let voice_state = self.inner.voice_state(*user_id, guild_id)?;

                        Some(VoiceState {
                            channel_id: Some(voice_state.channel_id()),
                            deaf: voice_state.deaf(),
                            guild_id: Some(voice_state.guild_id()),
                            member: self.member(guild_id, *user_id),
                            mute: voice_state.mute(),
                            self_deaf: voice_state.self_deaf(),
                            self_mute: voice_state.self_mute(),
                            self_stream: voice_state.self_stream(),
                            self_video: voice_state.self_video(),
                            session_id: voice_state.session_id().to_string(),
                            suppress: voice_state.suppress(),
                            user_id: voice_state.user_id(),
                            request_to_speak_timestamp: voice_state.request_to_speak_timestamp(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn threads_in_guild(&self, guild_id: Id<GuildMarker>) -> Vec<Channel> {
        self.inner
            .guild_channels(guild_id)
            .map(|reference| {
                reference
                    .iter()
                    .filter_map(|channel_id| {
                        let channel = self.inner.channel(*channel_id)?;

                        if channel.kind.is_thread() {
                            Some(channel.value().clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_guild_payloads<'a>(
        &'a self,
        sequence: &'a mut usize,
    ) -> impl Iterator<Item = String> + 'a {
        self.inner.iter().guilds().map(move |guild| {
            *sequence += 1;

            if guild.unavailable() == Some(true) {
                to_string(&Payload {
                    d: GuildDelete {
                        id: guild.id(),
                        unavailable: Some(true),
                    },
                    op: OpCode::Dispatch,
                    t: "GUILD_DELETE",
                    s: *sequence,
                })
                .unwrap()
            } else {
                let guild_channels = self.channels_in_guild(guild.id());
                let presences = self.presences_in_guild(guild.id());
                let emojis = self.emojis_in_guild(guild.id());
                let members = self.members_in_guild(guild.id());
                let roles = self.roles_in_guild(guild.id());
                let scheduled_events = self.scheduled_events_in_guild(guild.id());
                let stage_instances = self.stage_instances_in_guild(guild.id());
                let stickers = self.stickers_in_guild(guild.id());
                let voice_states = self.voice_states_in_guild(guild.id());
                let threads = self.threads_in_guild(guild.id());

                let new_guild = Guild {
                    afk_channel_id: guild.afk_channel_id(),
                    afk_timeout: guild.afk_timeout(),
                    application_id: guild.application_id(),
                    approximate_member_count: None, // Only present in with_counts HTTP endpoint
                    banner: guild.banner().map(ToOwned::to_owned),
                    approximate_presence_count: None, // Only present in with_counts HTTP endpoint
                    channels: guild_channels,
                    default_message_notifications: guild.default_message_notifications(),
                    description: guild.description().map(ToString::to_string),
                    discovery_splash: guild.discovery_splash().map(ToOwned::to_owned),
                    emojis,
                    explicit_content_filter: guild.explicit_content_filter(),
                    features: guild.features().cloned().collect(),
                    guild_scheduled_events: scheduled_events,
                    icon: guild.icon().map(ToOwned::to_owned),
                    id: guild.id(),
                    joined_at: guild.joined_at(),
                    large: guild.large(),
                    max_members: guild.max_members(),
                    max_presences: guild.max_presences(),
                    max_stage_video_channel_users: guild.max_stage_video_channel_users(),
                    max_video_channel_users: guild.max_video_channel_users(),
                    member_count: guild.member_count(),
                    members,
                    mfa_level: guild.mfa_level(),
                    name: guild.name().to_string(),
                    nsfw_level: guild.nsfw_level(),
                    owner_id: guild.owner_id(),
                    owner: guild.owner(),
                    permissions: guild.permissions(),
                    public_updates_channel_id: guild.public_updates_channel_id(),
                    preferred_locale: guild.preferred_locale().to_string(),
                    premium_progress_bar_enabled: guild.premium_progress_bar_enabled(),
                    premium_subscription_count: guild.premium_subscription_count(),
                    premium_tier: guild.premium_tier(),
                    presences,
                    roles,
                    rules_channel_id: guild.rules_channel_id(),
                    safety_alerts_channel_id: guild.safety_alerts_channel_id(),
                    splash: guild.splash().map(ToOwned::to_owned),
                    stage_instances,
                    stickers,
                    system_channel_flags: guild.system_channel_flags(),
                    system_channel_id: guild.system_channel_id(),
                    threads,
                    unavailable: Some(false),
                    vanity_url_code: guild.vanity_url_code().map(ToString::to_string),
                    verification_level: guild.verification_level(),
                    voice_states,
                    widget_channel_id: guild.widget_channel_id(),
                    widget_enabled: guild.widget_enabled(),
                };

                to_string(&Payload {
                    d: new_guild,
                    op: OpCode::Dispatch,
                    t: "GUILD_CREATE",
                    s: *sequence,
                })
                .unwrap()
            }
        })
    }
}
