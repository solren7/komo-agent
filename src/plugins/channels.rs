//! Ingress channels as plugins. Each mounts only when its `[channels.*]`
//! table is enabled and credentialed (`ready()`); `validate_gateway` has
//! already made an enabled-but-misconfigured channel fatal, so a `ready()`
//! miss here simply means "not configured".
//!
//! Registration order in `builtin()` is the `home_chat` fallback priority
//! (feishu first), preserving pre-plugin behavior.

use std::sync::Arc;

use async_trait::async_trait;

use super::{ChannelCx, ChannelRegistry, FailureMode, Plugin};
use crate::domain::gateway::WeChatLogin;
use crate::infra::messaging::{
    feishu::{FeishuChannel, FeishuSender},
    telegram::{TelegramChannel, TelegramSender},
    wechat::{WeChatChannel, WeChatQrLogin, WeChatSender, build_bot},
};

pub struct FeishuPlugin;

#[async_trait]
impl Plugin for FeishuPlugin {
    fn name(&self) -> &'static str {
        "feishu"
    }

    fn failure(&self) -> FailureMode {
        FailureMode::Fatal
    }

    async fn setup_channels(
        &self,
        reg: &mut ChannelRegistry,
        cx: &ChannelCx<'_>,
    ) -> anyhow::Result<()> {
        let Some(cfg) = cx.config.runtime.feishu.ready() else {
            return Ok(());
        };
        let sender = Arc::new(FeishuSender::new(
            cfg.app_id.clone(),
            cfg.app_secret.clone(),
        ));
        reg.sender("feishu", sender.clone());
        if let Some(chat) = &cfg.home_chat {
            reg.home_candidate(format!("feishu:{chat}"));
        }
        reg.channel(
            "feishu",
            Box::new(FeishuChannel::new(sender, cfg, cx.pairings.clone())),
        );
        Ok(())
    }
}

pub struct TelegramPlugin;

#[async_trait]
impl Plugin for TelegramPlugin {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn failure(&self) -> FailureMode {
        FailureMode::Fatal
    }

    async fn setup_channels(
        &self,
        reg: &mut ChannelRegistry,
        cx: &ChannelCx<'_>,
    ) -> anyhow::Result<()> {
        let Some(cfg) = cx.config.runtime.telegram.ready() else {
            return Ok(());
        };
        let sender = Arc::new(TelegramSender::new(cfg.bot_token.clone()));
        reg.sender("telegram", sender.clone());
        if let Some(chat) = &cfg.home_chat {
            reg.home_candidate(format!("telegram:{chat}"));
        }
        reg.channel(
            "telegram",
            Box::new(TelegramChannel::new(sender, cfg, cx.pairings.clone())),
        );
        Ok(())
    }
}

pub struct WeChatPlugin;

#[async_trait]
impl Plugin for WeChatPlugin {
    fn name(&self) -> &'static str {
        "wechat"
    }

    fn failure(&self) -> FailureMode {
        FailureMode::Fatal
    }

    async fn setup_channels(
        &self,
        reg: &mut ChannelRegistry,
        cx: &ChannelCx<'_>,
    ) -> anyhow::Result<()> {
        let Some(cfg) = cx.config.runtime.wechat.ready() else {
            return Ok(());
        };
        let cred_path = komo_config::wechat_cred_path();
        // One bot instance shared between the sender and the channel so the
        // channel's poll loop populates the context-token map the sender reads.
        let bot = build_bot(&cred_path);
        reg.sender("wechat", Arc::new(WeChatSender::new(bot.clone())));
        if let Some(chat) = &cfg.home_chat {
            reg.home_candidate(format!("wechat:{chat}"));
        }
        // Shared between the login coordinator (`/wechat login`) and the
        // channel: a successful login pulses this so the channel starts
        // polling without a restart.
        let ready = Arc::new(tokio::sync::Notify::new());
        let provisioning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        reg.wechat_login = Some(Arc::new(WeChatQrLogin::new(
            cred_path.clone(),
            ready.clone(),
            bot.clone(),
            provisioning.clone(),
        )) as Arc<dyn WeChatLogin>);
        reg.channel(
            "wechat",
            Box::new(WeChatChannel::new(
                bot,
                cfg,
                cred_path,
                ready,
                provisioning,
                cx.pairings.clone(),
            )),
        );
        Ok(())
    }
}
