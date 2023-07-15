mod markdown;

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use bimap::BiMap;
use failure::format_err;
use serenity::async_trait;
use serenity::builder::{CreateEmbed, CreateMessage};
use serenity::framework::standard::macros::{command, group};
use serenity::framework::standard::{Args, CommandResult, StandardFramework};
use serenity::futures::StreamExt;
use serenity::http::CacheHttp;
use serenity::model::channel::Message;
use serenity::model::event::Event;
use serenity::model::gateway::Ready;
use serenity::model::guild::{Guild, Member};
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::model::user::User;
use serenity::prelude::*;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

#[derive(Default, Debug)]
struct IntroState {
    intro_channel: ChannelId,
    guild: GuildId,
    data: RwLock<IntroStateData>,
}

#[derive(Default, Debug)]
struct IntroStateData {
    intros: HashMap<UserId, MessageId>,
    msg_cache: HashMap<MessageId, Message>,
    // guild local display names
    displaynames: BiMap<String, UserId>,
    usernames: BiMap<String, UserId>,
}

impl IntroState {
    async fn run(&self, ctx: &Context) -> Result<(), failure::Error> {
        // first, initialize all messages
        self.refresh_intro_channel(ctx).await?;

        // print some stats every once in a while for lack of a better thing to do
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            {
                let data = self.data.read().await;
                info!(
                    "guild {} is tracking {} users / {} intros",
                    self.guild,
                    data.displaynames.len(),
                    data.intros.len(),
                );
            }
        }
    }

    async fn refresh_intro_channel(&self, ctx: &Context) -> Result<(), failure::Error> {
        // first, fetch all members so they're cached below.
        // This lets us make batch calls instead of a bunch of O(1) calls with 'msg.member' below
        // due to the cache
        info!("initializing members for guild: {}", self.guild);
        let mut members = self.guild.members_iter(ctx).boxed();
        while let Some(mem) = members.next().await {
            self.update_user(&mem?).await;
        }
        info!(
            "refreshing channel for guild {}: {}",
            self.guild, self.intro_channel
        );

        let mut messages = self.intro_channel.messages_iter(ctx).boxed();
        let mut count = 0;
        while let Some(msgr) = messages.next().await {
            debug!(
                "refreshing intro channel {}; count={}",
                self.intro_channel, count
            );
            let msg = msgr?;
            {
                let mut l = self.data.write().await;
                l.intros.entry(msg.author.id).or_insert(msg.id);
                l.msg_cache.entry(msg.id).or_insert(msg);
            }
            count += 1;
        }
        info!(
            "refreshed {} messages in intro channel for guild {}: {}",
            count, self.guild, self.intro_channel
        );
        Ok(())
    }

    async fn update_user(&self, m: &Member) {
        // username if they have one
        debug!(
            "updating user: {}, {:?}, {}",
            m.user.id,
            m.user.name,
            m.display_name()
        );
        let mut l = self.data.write().await;
        if m.user.discriminator.is_none() {
            if let Some(cur_global) = l.usernames.get_by_right(&m.user.id) {
                if cur_global != &m.user.name {
                    l.usernames.remove_by_right(&m.user.id);
                }
            }
            l.usernames.insert(m.user.name.clone(), m.user.id);
        }

        // displayname either way
        let nick = m.display_name().to_lowercase();
        if let Some(cur_nick) = l.displaynames.get_by_right(&m.user.id) {
            if cur_nick != &nick {
                // otherwise, update; delete then re-add below with the new nick
                l.displaynames.remove_by_right(&m.user.id);
            }
        }
        l.displaynames.insert(nick, m.user.id);
    }

    async fn get_user_by_string(
        &self,
        ctx: impl CacheHttp,
        name: &str,
    ) -> serenity::Result<Option<User>> {
        let uid = {
            let l = self.data.read().await;
            l.usernames
                .get_by_left(name)
                .cloned()
                .or_else(|| l.displaynames.get_by_left(&name.to_lowercase()).cloned())
        };

        if let Some(id) = uid {
            if let Some(c) = ctx.cache() {
                if let Some(u) = c.user(id) {
                    return Ok(Some(u.clone()));
                }
            }
            warn!("having to make a network request for {}", id);
            return ctx.http().get_user(id).await.map(Some);
        }
        Ok(None)
    }

    async fn get_intro_message(&self, user_id: UserId) -> Option<Message> {
        let l = self.data.read().await;
        l.intros
            .get(&user_id)
            .and_then(|mid| l.msg_cache.get(mid))
            .cloned()
    }

    async fn update_intro_message(&self, usr: UserId, msg: Message) {
        let mut l = self.data.write().await;
        l.intros.entry(usr).or_insert(msg.id);
        l.msg_cache.insert(msg.id, msg);
    }

    async fn get_user_displayname(&self, user_id: UserId) -> Option<String> {
        let l = self.data.read().await;
        l.displaynames
            .get_by_right(&user_id)
            .or_else(|| l.usernames.get_by_right(&user_id))
            .cloned()
    }
}

struct State;

impl TypeMapKey for State {
    type Value = Arc<RwLock<HashMap<GuildId, Arc<IntroState>>>>;
}

#[group]
#[commands(intro)]
struct General;

struct Handler {}

#[async_trait]
impl EventHandler for Handler {
    // For usage, we use an arbitrary message handler.
    // The intended usage is '@bot !usage', but we just check for ending with usage
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.id == ctx.cache.current_user().id {
            // ignore ourselves
            debug!("ignoring self");
            return;
        }
        if msg.mentions_me(&ctx).await.unwrap_or(false) && msg.content.ends_with("usage") {
            if let Err(e) = command_usage(&ctx, &msg).await {
                error!("error running usage: {:?}", e);
            }
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }

    // guild_create is called when we're added to a guild.
    // Use this to find the intro channel and watch for mesasges.
    async fn guild_create(&self, ctx: Context, guild: Guild, _: Option<bool>) {
        let chan = {
            match guild
                .channels(&ctx)
                .await
                .map_err(|e| e.into())
                .and_then(|channels| {
                    channels
                        .iter()
                        .find(|(_, c)| c.name() == "introductions")
                        .map(|(id, _)| id)
                        .cloned()
                        .ok_or_else(|| format_err!("could not find introductions channel"))
                }) {
                Ok(chan) => chan,
                Err(e) => {
                    error!("error handling guild {:?} create: {}", guild, e);
                    return;
                }
            }
        };

        tokio::spawn(async move {
            let state = {
                let rwl = {
                    let data = ctx.data.read().await;
                    data.get::<State>().unwrap().clone()
                };
                let mut all_state = rwl.write().await;
                if all_state.contains_key(&guild.id) {
                    return;
                }

                let state = Arc::new(IntroState {
                    intro_channel: chan,
                    guild: guild.id,
                    ..Default::default()
                });
                all_state.insert(guild.id, state.clone());
                state
            };

            let res = state.run(&ctx).await;
            if let Err(e) = res {
                error!("error running intro state: {}", e);
            }
        });
    }

    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
        println!("cache ready!");
        // wait for messages for the intro channels
        let ctx = Arc::new(ctx);

        let mut intro_channels: Vec<(GuildId, ChannelId)> = Vec::new();
        for guild in guilds {
            let intro_channel =
                match guild
                    .channels(&ctx)
                    .await
                    .map_err(|e| e.into())
                    .and_then(|channels| {
                        channels
                            .iter()
                            .find(|(_, c)| c.name() == "introductions")
                            .map(|(id, _)| id)
                            .cloned()
                            .ok_or_else(|| format_err!("could not find introductions channel"))
                    }) {
                    Ok(chan) => chan,
                    Err(e) => {
                        error!("error handling guild {} create: {}", guild, e);
                        continue;
                    }
                };
            intro_channels.push((guild, intro_channel));
        }

        // for each one, setup intro state
        for chan in intro_channels {
            let ctx = Arc::clone(&ctx);
            tokio::spawn(async move {
                let rwl = {
                    let data = ctx.data.read().await;
                    data.get::<State>().unwrap().clone()
                };
                let state = {
                    let mut all_state = rwl.write().await;
                    if all_state.contains_key(&chan.0) {
                        return;
                    }

                    let state = Arc::new(IntroState {
                        intro_channel: chan.1,
                        guild: chan.0,
                        ..Default::default()
                    });
                    all_state.insert(chan.0, state.clone());
                    state
                };

                let res = state.run(&ctx).await;
                if let Err(e) = res {
                    error!("error running intro state: {}", e);
                }
            });
        }
    }

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
            let rwl = {
                let data = ctx.data.read().await;
                data.get::<State>().unwrap().clone()
            };
            let mut all_state = rwl.write().await;
            all_state
                .entry(guild_id)
                .or_insert(Default::default())
                .clone()
        };

        // cache intro messages
        if state.intro_channel == update.channel_id {
            ctx.cache.update(&mut update);
        }
    }
}

struct RawHandler;

impl RawHandler {
    async fn member_event(&self, ctx: Context, guild_id: GuildId, mem: &Member) {
        // track usernames
        let data = ctx.data.read().await;
        let rwl = data.get::<State>().unwrap().clone();
        let all_state = rwl.read().await;
        let state: &IntroState = match all_state.get(&guild_id) {
            None => return,
            Some(s) => s,
        };
        state.update_user(mem).await;
    }

    async fn channel_message(
        &self,
        ctx: Context,
        guild_id: GuildId,
        channel_id: ChannelId,
        user: UserId,
        msg: Message,
    ) {
        // cache channel messages for the intro channel only
        let rwl = {
            let data = ctx.data.read().await;
            data.get::<State>().unwrap().clone()
        };
        let all_state = rwl.read().await;
        let state: &IntroState = match all_state.get(&guild_id) {
            None => return,
            Some(s) => s,
        };

        if state.intro_channel != channel_id {
            return;
        }
        // cache if it's intro and we track this guild
        state.update_intro_message(user, msg).await;
    }

    async fn get_message(&self, ctx: &Context, guild: GuildId, msg: MessageId) -> Option<Message> {
        let rwl = {
            let data = ctx.data.read().await;
            data.get::<State>().unwrap().clone()
        };
        let state = {
            let all_state = rwl.read().await;
            match all_state.get(&guild) {
                None => return None,
                Some(s) => s.clone(),
            }
        };
        let l = state.data.read().await;
        l.msg_cache.get(&msg).cloned()
    }
}

#[async_trait]
impl RawEventHandler for RawHandler {
    async fn raw_event(&self, ctx: Context, raw_ev: serenity::model::event::Event) {
        // This could be DRY'd with a macro
        match raw_ev {
            // cache messages, but only:
            // 1. In the intro channel, or 2. for username info so we can track usernames
            Event::MessageCreate(ev) => {
                if let Some(gid) = ev.message.guild_id {
                    self.channel_message(
                        ctx,
                        gid,
                        ev.message.channel_id,
                        ev.message.author.id,
                        ev.message,
                    )
                    .await;
                }
            }
            Event::MessageUpdate(ev) => {
                if let (Some(a), Some(gid)) = (&ev.author, ev.guild_id) {
                    let mut msg = match self.get_message(&ctx, gid, ev.id).await {
                        None => {
                            error!("no original message: {:?}", ev);
                            return;
                        }
                        Some(m) => m,
                    };
                    // hacky, but we can only update a message if we have the original, and the
                    // builtin cache doesn't work for us, so we're forced to do this
                    ev.apply_to_message(&mut msg);
                    self.channel_message(ctx, gid, ev.channel_id, a.id, msg)
                        .await;
                }
            }
            Event::MessageDelete(_ev) => {
                // TODO
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
                self.member_event(ctx, ev.member.guild_id, &ev.member).await;
            }
            Event::GuildMemberUpdate(mut ev) => {
                if let Some(mem) = ctx.cache.update(&mut ev) {
                    self.member_event(ctx, ev.guild_id, &mem).await;
                }
            }
            Event::GuildMembersChunk(mut ev) => {
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
    // console_subscriber::init();
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let framework = StandardFramework::new().group(&GENERAL_GROUP);
    framework.configure(|c| c.with_whitespace(true).prefix("!"));

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler {})
        .raw_event_handler(RawHandler)
        .framework(framework)
        .await
        .expect("Err creating client");
    {
        let mut data = client.data.write().await;
        data.insert::<State>(Arc::new(Default::default()));
    }

    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}

#[command]
async fn intro(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
    debug!("got intro command: {:?}", args);
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
        None => {
            warn!("message had no guild id: {:?}", msg);
            return Ok(());
        }
        Some(id) => id,
    };

    let state: Arc<IntroState> = {
        let rwl = {
            let data = ctx.data.read().await;
            data.get::<State>().unwrap().clone()
        };
        let all_state = rwl.read().await;
        match all_state.get(&guild_id) {
            None => {
                warn!("could not get guild for id: {}", guild_id);
                return Ok(());
            }
            Some(s) => s.clone(),
        }
    };
    debug!("got guild info for intro command: {}", guild_id);

    let target_user = {
        if msg.mentions.len() == 1 {
            // get that user's intro
            msg.mentions.first().unwrap().clone()
        } else {
            let arg = args.remains().unwrap();
            match state.get_user_by_string(ctx, arg).await? {
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

    let nick = state
        .get_user_displayname(target_user.id)
        .await
        .unwrap_or(target_user.name.clone());

    let intro_msg = match state.get_intro_message(target_user.id).await {
        None => {
            if let Err(e) = msg
                .reply(
                    ctx,
                    format!("intro seems to have vanished for {}", target_user.name),
                )
                .await
            {
                error!("error replying: {:?}", e);
            }
            return Ok(());
        }
        Some(m) => m,
    };
    let mut intro_embed = CreateEmbed::new()
        .title(format!("**{nick}**"))
        .color(0x7598ff)
        .description(&intro_msg.content)
        .field("", intro_msg.link(), true);
    if let Some(url) = target_user.avatar_url() {
        intro_embed = intro_embed.thumbnail(url);
    }

    debug!("replying with intro: {:?}", intro_embed);

    let is_too_long = match msg
        .channel_id
        .send_message(ctx, CreateMessage::new().embed(intro_embed))
        .await
    {
        Err(SerenityError::Http(e)) => match e {
            serenity::http::HttpError::UnsuccessfulRequest(e) => e
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
        Ok(_) => {
            debug!("sent reply");
            false
        }
    };
    // we got an error telling us the embed was too long, give it another go as a normal
    // message
    if is_too_long {
        if let Err(e) = msg
            .channel_id
            .send_message(
                ctx,
                CreateMessage::new().content(format!("**{nick}**\n{}", intro_msg.content)),
            )
            .await
        {
            error!("error sending long message: {}", e);
        }
    }
    debug!("sent plaintext message");
    Ok(())
}

async fn command_usage(ctx: &Context, msg: &Message) -> CommandResult {
    msg.reply(ctx, r#"Usage:
!intro <username>   - reply with the introduction post for the mentioned user. Accepted formats for the user are a mention ('@user'), or the user's chosen display name.
@introbot usage     - this message
"#).await?;
    Ok(())
}
