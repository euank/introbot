use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bimap::BiMap;
use tracing::{debug, error};

use serenity::async_trait;
use serenity::framework::standard::macros::{command, group};
use serenity::framework::standard::{Args, CommandResult, StandardFramework};
use serenity::futures::StreamExt;
use serenity::http::CacheHttp;
use serenity::model::channel::Message;
use serenity::model::event::Event;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::model::user::User;
use serenity::prelude::*;
use tokio::sync::RwLock;

#[derive(Default, Debug)]
struct IntroState {
    intros: RwLock<HashMap<UserId, Message>>,
    displaynames: RwLock<BiMap<String, UserId>>,
    intro_channel: std::sync::atomic::AtomicU64,
}

impl IntroState {
    async fn update_user(&self, u: User, nick: Option<String>) {
        let mut l = self.displaynames.write().await;
        let nick = nick.unwrap_or(u.name.clone()).to_lowercase();
        if let Some(cur_nick) = l.get_by_right(&u.id) {
            if cur_nick == &nick {
                return;
            }
            // otherwise, update; delete then re-add below with the new nick
            l.remove_by_right(&u.id);
        }
        l.insert(nick, u.id);
    }

    async fn get_user_by_displayname(
        &self,
        ctx: impl CacheHttp,
        name: &str,
    ) -> serenity::Result<Option<User>> {
        if let Some(id) = {
            let lock = self.displaynames.read().await;
            lock.get_by_left(&name.to_lowercase()).cloned()
        } {
            if let Some(c) = ctx.cache() {
                if let Some(u) = c.user(id) {
                    return Ok(Some(u));
                }
            }
            return ctx.http().get_user(*id.as_u64()).await.map(Some);
        }
        Ok(None)
    }

    async fn refresh_intro_channel(
        &self,
        ctx: impl CacheHttp,
        guild_id: GuildId,
    ) -> serenity::Result<bool> {
        let channels = ctx.http().get_channels(*guild_id.as_u64()).await?;
        let intro_channel = match channels
            .iter()
            .find(|el| el.name == "introductions")
            .cloned()
        {
            None => return Ok(false),
            Some(c) => c,
        };
        // Otherwise, we have an intro channel which was not cached. Populate the full list of
        // intros before caching it.
        let mut messages = intro_channel.id.messages_iter(ctx.http()).boxed();
        while let Some(msgr) = messages.next().await {
            let msg = msgr?;
            self.intros
                .write()
                .await
                .entry(msg.author.id)
                .or_insert(msg.clone());

            if let Some(pm) = msg.member {
                self.update_user(msg.author, pm.nick).await;
            } else if !self
                .displaynames
                .read()
                .await
                .contains_right(&msg.author.id)
            {
                // displaynane is already there, assume it's up to date
                let nick = msg
                    .author
                    .nick_in(&ctx, guild_id)
                    .await
                    .unwrap_or(msg.author.name.clone());
                self.displaynames
                    .write()
                    .await
                    .insert(nick.to_lowercase(), msg.author.id);
            }
        }

        debug!(
            "got intro channel for guild {}: {}",
            guild_id, intro_channel
        );
        self.intro_channel
            .store(*intro_channel.id.as_u64(), Ordering::Relaxed);
        Ok(true)
    }

    async fn get_intro_channel(
        &self,
        ctx: impl CacheHttp,
        guild_id: GuildId,
    ) -> serenity::Result<Option<ChannelId>> {
        let c = self.intro_channel.load(Ordering::Relaxed);
        if c != 0 {
            return Ok(Some(ChannelId::from(c)));
        }

        let channels = ctx.http().get_channels(*guild_id.as_u64()).await?;
        let intro_channel = match channels
            .iter()
            .find(|el| el.name == "introductions")
            .cloned()
        {
            None => return Ok(None),
            Some(c) => c,
        };
        self.refresh_intro_channel(&ctx, guild_id).await?;

        debug!(
            "got intro channel for guild {}: {}",
            guild_id, intro_channel
        );
        Ok(Some(ChannelId::from(
            self.intro_channel.load(Ordering::Relaxed),
        )))
    }

    async fn get_intro_message(
        &self,
        ctx: &impl CacheHttp,
        guild_id: GuildId,
        msg_id: MessageId,
    ) -> serenity::Result<Option<Message>> {
        let chan = match self.get_intro_channel(ctx, guild_id).await? {
            None => return Ok(None),
            Some(c) => c,
        };

        if let Some(msg) = ctx.cache().and_then(|c| c.message(chan, msg_id)) {
            return Ok(Some(msg));
        }

        // TODO: map not found to Ok(None)
        let updated_message = chan.message(ctx.http(), msg_id).await?;
        Ok(Some(updated_message))
    }
}

struct State;

impl TypeMapKey for State {
    type Value = Arc<RwLock<HashMap<GuildId, Arc<IntroState>>>>;
}

#[group]
#[commands(intro)]
struct General;

struct Handler {
    is_loop_running: AtomicBool,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.id == ctx.cache.current_user_id() {
            // ignore ourselves
            debug!("ignoring self");
            return;
        }
        if msg.mentions_me(&ctx).await.unwrap_or(false) && msg.content.ends_with("usage") {
            if let Err(e) = command_usage(&ctx, &msg).await {
                error!("error running usage: {:?}", e);
            }
        }

        let guild_id = match msg.guild_id {
            None => {
                debug!("message with no guild_id");
                return;
            }
            Some(id) => id,
        };
        let state = {
            let data = ctx.data.write().await;
            let rwl = data.get::<State>().unwrap().clone();
            let mut all_state = rwl.write().await;
            all_state
                .entry(guild_id)
                .or_insert(Default::default())
                .clone()
        };

        // If it's an intro, save it
        if state
            .get_intro_channel(&ctx, guild_id)
            .await
            .unwrap_or(None)
            == Some(msg.channel_id)
        {
            state
                .intros
                .write()
                .await
                .insert(msg.author.id, msg.clone());
        }
    }

    // The below all update the serenity cache. TODO: this could be way more DRY
    async fn message_update(
        &self,
        ctx: Context,
        _: Option<Message>,
        _: Option<Message>,
        mut update: serenity::model::event::MessageUpdateEvent,
    ) {
        let guild_id = match update.guild_id {
            None => {
                debug!("message with no guild_id");
                return;
            }
            Some(id) => id,
        };

        let state = {
            let data = ctx.data.write().await;
            let rwl = data.get::<State>().unwrap().clone();
            let mut all_state = rwl.write().await;
            all_state
                .entry(guild_id)
                .or_insert(Default::default())
                .clone()
        };

        // cache intro messages
        if state
            .get_intro_channel(&ctx, guild_id)
            .await
            .unwrap_or(None)
            == Some(update.channel_id)
        {
            ctx.cache.update(&mut update);
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
        // Sleep to give the cache time to ready up this below
        tokio::time::sleep(Duration::from_secs(2)).await;

        let ctx = Arc::new(ctx);
        if !self.is_loop_running.load(Ordering::Relaxed) {
            let ctx = Arc::clone(&ctx);
            tokio::spawn(async move {
                loop {
                    // update each guild, basically initialize the username caches
                    // TODO: pagination, we're doing 100 now because we're only in 2 so who cares
                    // right
                    let guilds = match ctx.http.get_guilds(None, Some(100)).await {
                        Err(e) => {
                            error!("error getting guilds: {:?}", e);
                            tokio::time::sleep(Duration::from_secs(120)).await;
                            continue;
                        }
                        Ok(g) => g,
                    };
                    for guild in guilds {
                        let state = {
                            let data = ctx.data.write().await;
                            let rwl = data.get::<State>().unwrap().clone();
                            let mut all_state = rwl.write().await;
                            all_state
                                .entry(guild.id)
                                .or_insert(Default::default())
                                .clone()
                        };
                        if let Err(e) = state.refresh_intro_channel(&ctx, guild.id).await {
                            error!("error: {}", e);
                        }
                    }
                    debug!("updated guild intro caches");
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
            });
            self.is_loop_running.swap(true, Ordering::Relaxed);
        }
    }
}

struct RawHandler;

#[async_trait]
impl RawEventHandler for RawHandler {
    async fn raw_event(&self, ctx: Context, ev: serenity::model::event::Event) {
        debug!("Raw event: {:?}", ev);
        let guild_id = match ev.guild_id() {
            serenity::model::event::RelatedId::Some(id) => Some(id),
            _ => None,
        };
        let channel_id = match ev.channel_id() {
            serenity::model::event::RelatedId::Some(id) => Some(id),
            _ => None,
        };

        let (is_intro_channel, state) = if let Some(guild_id) = guild_id {
            let s = {
                let data = ctx.data.write().await;
                let rwl = data.get::<State>().unwrap().clone();
                let mut all_state = rwl.write().await;
                all_state
                    .entry(guild_id)
                    .or_insert(Default::default())
                    .clone()
            };

            let is_intro = s.get_intro_channel(&ctx, guild_id).await.unwrap_or(None) == channel_id;
            (is_intro, Some(s))
        } else {
            (false, None)
        };

        // This could be DRY'd with a macro
        match ev {
            // cache messages, but only:
            // 1. In the intro channel, or 2. for username info so we can track usernames
            Event::MessageCreate(mut ev) => {
                if is_intro_channel {
                    ctx.cache.update(&mut ev);
                }
            }
            Event::MessageUpdate(mut ev) => {
                if is_intro_channel {
                    ctx.cache.update(&mut ev);
                }
            }
            Event::ChannelCreate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::ChannelDelete(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::ChannelPinsUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::ChannelUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildCreate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildDelete(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildEmojisUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildMemberRemove(mut ev) => {
                ctx.cache.update(&mut ev);
                // TODO; delete em I guess? Idk
            }
            Event::GuildMemberAdd(mut ev) => {
                ctx.cache.update(&mut ev);
                if let Some(s) = state {
                    s.update_user(ev.member.user, ev.member.nick).await;
                }
            }
            Event::GuildMemberUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
                if let Some(s) = state {
                    s.update_user(ev.user, ev.nick).await;
                }
            }
            Event::GuildMembersChunk(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildUnavailable(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::PresenceUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::PresencesReplace(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::UserUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            _ => return,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c.with_whitespace(true).prefix("!"))
        .group(&GENERAL_GROUP);

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler {
            is_loop_running: AtomicBool::new(false),
        })
        .raw_event_handler(RawHandler)
        .framework(framework)
        .await
        .expect("Err creating client");
    {
        // Open the data lock in write mode, so keys can be inserted to it.
        let mut data = client.data.write().await;

        data.insert::<State>(Arc::new(Default::default()));
    }

    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}

#[command]
async fn intro(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
    if args.is_empty() {
        if let Err(e) = msg
            .reply(
                ctx,
                "error: !intro requires an argument (a @user or display name)",
            )
            .await
        {
            error!("error replying: {:?}", e);
        }
        return Ok(());
    }

    if msg.mentions.len() > 1 {
        if let Err(e) = msg
            .reply(
                ctx,
                "error: !intro requires an argument (a @user or display name)",
            )
            .await
        {
            error!("error replying: {:?}", e);
        }
        return Ok(());
    }
    let guild_id = match msg.guild_id {
        None => return Ok(()),
        Some(id) => id,
    };

    let data = ctx.data.read().await;
    let rwl = data.get::<State>().unwrap().clone();
    let mut all_state = rwl.write().await;
    let state: &IntroState = match all_state.get_mut(&guild_id) {
        None => return Ok(()),
        Some(s) => s,
    };

    let target_user = {
        if msg.mentions.len() == 1 {
            // get that user's intro
            msg.mentions.first().unwrap().clone()
        } else {
            let arg = args.remains().unwrap();
            match state.get_user_by_displayname(ctx, arg).await? {
                None => {
                    if let Err(e) = msg
                        .reply(ctx, format!("no user with display name {arg}"))
                        .await
                    {
                        error!("error replying: {:?}", e);
                    }
                    return Ok(());
                }
                Some(u) => u,
            }
        }
    };

    let nick = target_user
        .nick_in(&ctx, guild_id)
        .await
        .unwrap_or(target_user.name.clone());
    let intro_msg = match state.intros.read().await.get(&target_user.id).cloned() {
        None => {
            if let Err(e) = msg
                .reply(ctx, format!("no intro for {}", target_user.name))
                .await
            {
                error!("error replying: {:?}", e);
            }
            return Ok(());
        }
        Some(intro_msg) => intro_msg,
    };
    let intro_msg = match state.get_intro_message(ctx, guild_id, intro_msg.id).await? {
        None => {
            if let Err(e) = msg
                .reply(ctx, format!("intro seems to have vanished for {nick}"))
                .await
            {
                error!("error replying: {:?}", e);
            }
            return Ok(());
        }
        Some(m) => m,
    };
    let is_too_long = match msg
        .channel_id
        .send_message(ctx, |m| {
            m.embed(|e| {
                let e = e
                    .title(format!("**{nick}**"))
                    .color(0x7598ff)
                    .description(&format!(
                        "**Intro**\n{}\n_[link]({})_",
                        intro_msg.content.clone(),
                        intro_msg.link()
                    ));
                // TODO: technically we should markdown escape the intro_msg.content value before
                // embedding it since embedding supports markdown, but content is plaintext
                if let Some(url) = target_user.avatar_url() {
                    e.thumbnail(url)
                } else {
                    e
                }
            })
        })
        .await
    {
        Err(SerenityError::Http(e)) => match *e {
            serenity::http::error::Error::UnsuccessfulRequest(e) => e
                .error
                .errors
                .iter()
                .any(|e| e.code == "BASE_TYPE_MAX_LENGTH"),
            _ => {
                error!("error sending: {:?}", e);
                return Ok(());
            }
        },
        Err(e) => {
            error!("error sending: {:?}", e);
            return Ok(());
        }
        Ok(_) => false,
    };
    // we got an error telling us the embed was too long, give it another go as a normal
    // message
    if is_too_long {
        if let Err(e) = msg
            .channel_id
            .send_message(ctx, |mut cm| {
                if let Some(avatar) = target_user.avatar_url() {
                    let avatar_url: url::Url = avatar.parse().unwrap();
                    let old_pairs = avatar_url
                        .query_pairs()
                        .filter(|p| p.0 != "size")
                        .collect::<Vec<_>>();
                    let mut new_avatar_url = avatar_url.clone();
                    {
                        let mut qm = new_avatar_url.query_pairs_mut();
                        qm.clear();
                        for (k, v) in old_pairs {
                            qm.append_pair(&k, &v);
                        }
                        qm.append_pair("size", "72");
                    }
                    cm = cm.add_file(serenity::model::channel::AttachmentType::Image(
                        new_avatar_url,
                    ))
                };
                let mut msg = format!(
                    r#"**{nick}**
{}
"#,
                    intro_msg.content
                );
                // For embeds, discord limits us to 1024.
                // For content like this, we're limited to about 2k, but we want to leave some space
                // for the avatar, so go a bit shorter. This works in practice.
                if msg.len() > 1950 {
                    msg.truncate(1950);
                    msg += " *truncated*";
                }
                cm.content(msg)
            })
            .await
        {
            error!("error sending long message: {}", e);
        }
    }

    Ok(())
}

async fn command_usage(ctx: &Context, msg: &Message) -> CommandResult {
    msg.reply(ctx, r#"Usage:
!intro <username>   - reply with the introduction post for the mentioned user. Accepted formats for the user are a mention ('@user'), or the user's chosen display name.
@introbot usage     - this message
"#).await?;
    Ok(())
}
