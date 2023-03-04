use std::collections::HashMap;
use std::env;
use std::sync::Arc;

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
use serenity::model::id::{ChannelId, GuildId, UserId, MessageId};
use serenity::model::user::User;
use serenity::prelude::*;
use tokio::sync::RwLock;

#[derive(Default, Debug)]
struct IntroState {
    intros: HashMap<UserId, Message>,
    displaynames: BiMap<String, UserId>,
    intro_channel: Option<ChannelId>,
}

impl IntroState {
    async fn get_user_by_displayname(
        &mut self,
        ctx: impl CacheHttp,
        guild_id: GuildId,
        name: &str,
    ) -> serenity::Result<Option<User>> {
        for try_refresh in [false, true] {
            // otherwise, not in the cache, try and find em
            if try_refresh {
                // second try, refresh first, then try the same thing
                self.refresh_intro_channel(&ctx, guild_id).await?;
            }
            if let Some(id) = self.displaynames.get_by_left(name) {
                if let Some(c) = ctx.cache() {
                    if let Some(u) = c.user(id) {
                        return Ok(Some(u));
                    }
                }
                return ctx.http().get_user(*id.as_u64()).await.map(Some);
            }
        }
        Ok(None)
    }

    async fn refresh_intro_channel(
        &mut self,
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
            let nick = msg.author.nick_in(&ctx, guild_id).await.unwrap_or(msg.author.name.clone());
            if !self.displaynames.contains_left(&nick) {
                self.displaynames
                    .insert(nick, msg.author.id);
            }
            self.intros.entry(msg.author.id).or_insert(msg);
        }

        debug!(
            "got intro channel for guild {}: {}",
            guild_id, intro_channel
        );
        self.intro_channel = Some(intro_channel.id);
        Ok(true)
    }


    async fn get_intro_channel(
        &mut self,
        ctx: impl CacheHttp,
        guild_id: GuildId,
    ) -> serenity::Result<Option<ChannelId>> {
        if self.intro_channel.is_some() {
            return Ok(self.intro_channel);
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
        // Otherwise, we have an intro channel which was not cached. Populate the full list of
        // intros before caching it.
        let mut messages = intro_channel.id.messages_iter(ctx.http()).boxed();
        while let Some(msgr) = messages.next().await {
            let msg = msgr?;
            let nick = msg.author.nick_in(&ctx, guild_id).await.unwrap_or(msg.author.name.clone());
            if !self.displaynames.contains_left(&nick) {
                self.displaynames
                    .insert(nick, msg.author.id);
            }
            self.intros.entry(msg.author.id).or_insert(msg);
        }

        debug!(
            "got intro channel for guild {}: {}",
            guild_id, intro_channel
        );
        self.intro_channel = Some(intro_channel.id);
        Ok(self.intro_channel)
    }

    async fn get_intro_message(&mut self, ctx: &impl CacheHttp, guild_id: GuildId, msg_id: MessageId) -> serenity::Result<Option<Message>> {
        let chan = match self.get_intro_channel(ctx, guild_id).await? {
            None => return Ok(None),
            Some(c) => c,
        };

        if let Some(cache) = ctx.cache() {
            if let Some(cached) = cache.message(chan, msg_id) {
                return Ok(Some(cached));
            }
        }

        // TODO: map not found to Ok(None)
        let updated_message = chan.message(ctx.http(), msg_id).await?;
        Ok(Some(updated_message))
    }
}

struct State;

impl TypeMapKey for State {
    type Value = Arc<RwLock<HashMap<GuildId, IntroState>>>;
}

#[group]
#[commands(intro)]
struct General;

struct Handler;

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
        let data = ctx.data.write().await;
        let rwl = data.get::<State>().unwrap().clone();
        let mut all_state = rwl.write().await;
        let state = all_state.entry(guild_id).or_insert(Default::default());

        // If it's an intro, save it
        if state
            .get_intro_channel(&ctx, guild_id)
            .await
            .unwrap_or(None)
            == Some(msg.channel_id)
        {
            state.intros.insert(msg.author.id, msg.clone());
            state.displaynames.insert(msg.author.nick_in(&ctx, guild_id).await.unwrap_or(msg.author.name), msg.author.id);
        }
    }

    // The below all update the serenity cache. TODO: this could be way more DRY
    async fn message_update(&self, ctx: Context, _: Option<Message>, _: Option<Message>, mut update: serenity::model::event::MessageUpdateEvent) {
        let guild_id = match update.guild_id {
            None => {
                debug!("message with no guild_id");
                return;
            }
            Some(id) => id,
        };

        let data = ctx.data.write().await;
        let rwl = data.get::<State>().unwrap().clone();
        let mut all_state = rwl.write().await;
        let state = all_state.entry(guild_id).or_insert(Default::default());

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

    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

struct RawHandler;

#[async_trait]
impl RawEventHandler for RawHandler {
    async fn raw_event(&self, ctx: Context, ev: serenity::model::event::Event) {
        let guild_id = match ev.guild_id() {
            serenity::model::event::RelatedId::Some(id) => id,
            _ => return,
        };
        let channel_id = match ev.channel_id() {
            serenity::model::event::RelatedId::Some(id) => Some(id),
            _ => None,
        };

        let data = ctx.data.write().await;
        let rwl = data.get::<State>().unwrap().clone();
        let mut all_state = rwl.write().await;
        let state = all_state.entry(guild_id).or_insert(Default::default());

        // cache messages, but only:
        // 1. In the intro channel, or 2. for username info so we can track usernames
        if state.get_intro_channel(&ctx, guild_id).await.unwrap_or(None) != channel_id {
            match ev {
                serenity::model::event::Event::MessageCreate(mut ev) => {
                    ctx.cache.update(&mut ev);
                },
                serenity::model::event::Event::MessageUpdate(mut ev) => {
                    ctx.cache.update(&mut ev);
                },
                _ => return,
            };
            return;
        }

        // Otherwise, no channel ID, so it could be a username update
        match ev {
            Event::UserUpdate(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildMemberAdd(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildMemberRemove(mut ev) => {
                ctx.cache.update(&mut ev);
            }
            Event::GuildMemberUpdate(mut ev) => {
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

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_PRESENCES;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler)
        .raw_event_handler(RawHandler)
        .framework(framework)
        // 3000, but we also manually update for the introductions channel specifically
        .cache_settings(|s| s.max_messages(3000))
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
    let state: &mut IntroState = match all_state.get_mut(&guild_id) {
        None => return Ok(()),
        Some(s) => s,
    };

    let target_user = {
        if msg.mentions.len() == 1 {
            // get that user's intro
            msg.mentions.first().unwrap().clone()
        } else {
            let arg = args.remains().unwrap();
            match state.get_user_by_displayname(ctx, guild_id, arg).await? {
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

    let nick = target_user.nick_in(&ctx, guild_id).await.unwrap_or(target_user.name.clone());
    let intro_msg = match state.intros.get(&target_user.id) {
        None => {
            if let Err(e) = msg
                .reply(ctx, format!("no intro for {}", target_user.name))
                .await
            {
                error!("error replying: {:?}", e);
            }
            return Ok(());
        }
        Some(intro_msg) => {
            intro_msg
        }
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
        },
        Some(m) => m,
    };
    let is_too_long = match msg
        .channel_id
        .send_message(ctx, |m| {
            m.embed(|e| {
                let e = e
                    .title(format!("**{nick}**"))
                    .color(0x7598ff)
                    .description(&format!("**Intro**\n{}\n_[link]({})_", intro_msg.content.clone(), intro_msg.link()));
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
        Err(SerenityError::Http(e)) => {
            match *e {
                serenity::http::error::Error::UnsuccessfulRequest(e) => {
                    e.error.errors.iter().any(|e| e.code == "BASE_TYPE_MAX_LENGTH")
                },
                _ => {
                    error!("error sending: {:?}", e);
                    return Ok(())
                },
            }
        },
        Err(e) => {
            error!("error sending: {:?}", e);
            return Ok(())
        }
        Ok(_) => false,
    };
    // we got an error telling us the embed was too long, give it another go as a normal
    // message
    if is_too_long {
        if let Err(e) = msg.channel_id.send_message(ctx, |mut cm| {
            if let Some(avatar) = target_user.avatar_url() {
                let avatar_url: url::Url = avatar.parse().unwrap();
                let old_pairs = avatar_url.query_pairs().filter(|p| p.0 != "size").collect::<Vec<_>>();
                let mut new_avatar_url = avatar_url.clone();
                {
                    let mut qm = new_avatar_url.query_pairs_mut();
                    qm.clear();
                    for (k, v) in old_pairs {
                        qm.append_pair(&k, &v);
                    }
                    qm.append_pair("size", "72");
                }
                cm = cm.add_file(
                    serenity::model::channel::AttachmentType::Image(
                        new_avatar_url,
                    )
                )
            };
            let mut msg = format!(r#"**{nick}**
{}
"#, intro_msg.content);
            // For embeds, discord limits us to 1024.
            // For content like this, we're limited to about 2k, but we want to leave some space
            // for the avatar, so go a bit shorter. This works in practice.
            if msg.len() > 1950 {
                msg.truncate(1950);
                msg += " *truncated*";
            }
            cm.content(msg)
        }).await {
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
