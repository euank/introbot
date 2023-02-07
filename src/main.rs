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
use serenity::model::gateway::{Presence, Ready};
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
        &self,
        ctx: impl CacheHttp,
        name: &str,
    ) -> serenity::Result<Option<User>> {
        let id = match self.displaynames.get_by_left(name) {
            None => return Ok(None),
            Some(id) => id,
        };
        if let Some(c) = ctx.cache() {
            if let Some(u) = c.user(id) {
                return Ok(Some(u));
            }
        }
        ctx.http().get_user(*id.as_u64()).await.map(Some)
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
            if !self.displaynames.contains_left(&msg.author.name) {
                self.displaynames
                    .insert(msg.author.name.clone(), msg.author.id);
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
    async fn presence_update(&self, ctx: Context, pres: Presence) {
        if let (Some(guild_id), Some(name)) = (pres.guild_id, pres.user.name) {
            // possible update to displayname
            let data = ctx.data.read().await;
            let rwl = data.get::<State>().unwrap().clone();
            let mut all_state = rwl.write().await;
            let state = all_state.entry(guild_id).or_insert(Default::default());

            let old_name = state.displaynames.get_by_right(&pres.user.id).cloned();

            match old_name {
                None => {
                    state.displaynames.insert(name, pres.user.id);
                }
                Some(old_name) if old_name != name => {
                    state.displaynames.remove_by_left(&old_name);
                    state.displaynames.insert(name, pres.user.id);
                }
                Some(_) => {}
            }
        }
    }

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
            state.displaynames.insert(msg.author.name, msg.author.id);
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
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
                .reply(ctx, format!("intro seems to have vanished for {}", target_user.name))
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
            m.content(format!("**{}**", target_user.name)).embed(|e| {
                let e = e
                    .title(format!("**{}**", target_user.name))
                    .color(0x7598ff)
                    .field("Intro", intro_msg.content.clone() + &format!("\n_[link]({})_", intro_msg.link()), false);
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
                cm = cm.add_file(
                    serenity::model::channel::AttachmentType::Image(
                        avatar.parse().unwrap()
                    )
                )
            };
            let mut msg = format!(r#"**{}**

{}
"#, target_user.name, intro_msg.content);
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
