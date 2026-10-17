//! Invitations and joins. The verifier words go to the client running the
//! request, and an invitation nobody is left to answer for is cancelled.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

use super::clients::{Client, Input, secret_input};
use super::{Service, lock, node_failure};
use crate::identity::DeviceId;
use crate::ids::InvitationId;
use crate::invitation::{InvitationState, InvitationToken, MAX_TOKEN_CHARS};
use crate::ipc::{Body, DeviceView, ErrorCode, Event, Failure};
use crate::limits::{INVITATION_LIFETIME, Limits, MAX_NAME_COLUMNS};
use crate::trust::TrustState;

impl Service {
    pub(super) async fn invite(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        alias: String,
    ) -> Result<Body, Failure> {
        let (invitation, token) = self
            .with_node(|n| n.invite(&alias))?
            .map_err(node_failure)?;

        lock(&self.routes)
            .pairing
            .insert(invitation, (client.clone(), id));
        let _route = PairingRoute {
            svc: self.clone(),
            invitation,
            cancel: true,
        };
        client.event(
            id,
            Event::Invitation {
                token: token.to_string(),
            },
        );

        let mut changes = self.cell.subscribe();
        let deadline = Instant::now() + INVITATION_LIFETIME + Limits::COMPILED.idle_timeout;

        loop {
            match self.with_node(|n| n.invitation(invitation))? {
                None => return Err(Failure::new(ErrorCode::Failed, "the invitation is gone")),
                Some((_, Some(Ok(device)))) => return self.device_body(device),
                Some((_, Some(Err(why)))) => return Err(Failure::new(ErrorCode::Refused, why)),
                Some((InvitationState::Open | InvitationState::Consumed, None)) => {}
                Some((state, None)) => {
                    return Err(Failure::new(
                        ErrorCode::Refused,
                        format!("the invitation ended: {}", invitation_ended(state)),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(ErrorCode::Refused, "the invitation expired"));
            }
            // Expiry changes nothing in the state, so look again each second.
            let _ = timeout(Duration::from_secs(1), changes.changed()).await;
        }
    }

    pub(super) async fn join(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        alias: String,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        self.with_node(|_| ())?;
        let secret = secret_input(inputs).await?;
        if secret.len() > MAX_TOKEN_CHARS {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "the invitation is too long",
            ));
        }

        let text = std::str::from_utf8(&secret)
            .map_err(|_| Failure::new(ErrorCode::Invalid, "the invitation is not text"))?
            .trim();
        let token = InvitationToken::parse(text)
            .map_err(|e| Failure::new(ErrorCode::Invalid, e.to_string()))?;
        lock(&self.routes)
            .pairing
            .insert(token.id, (client.clone(), id));
        let _route = PairingRoute {
            svc: self.clone(),
            invitation: token.id,
            cancel: false,
        };

        let joined = self
            .with_node(|n| n.start_join(text, &alias))?
            .map_err(node_failure)?;
        let device = joined
            .await
            .map_err(|_| Failure::new(ErrorCode::Stopping, "the device was locked"))?
            .map_err(node_failure)?;
        self.device_body(device)
    }

    pub(super) fn device_body(&self, device: DeviceId) -> Result<Body, Failure> {
        let s = self.cell.lock();
        let r = s
            .trust()
            .record(&device)
            .ok_or_else(|| Failure::new(ErrorCode::Failed, "the device was not saved"))?;
        Ok(Body::Device(DeviceView {
            alias: r.alias.as_str().to_owned(),
            fingerprint: r.device.fingerprint(),
            name: r.declared_name.line(MAX_NAME_COLUMNS).to_string(),
            trusted: matches!(r.state, TrustState::Trusted { .. }),
        }))
    }
}

pub(super) fn invitation_ended(state: InvitationState) -> &'static str {
    match state {
        InvitationState::Expired => "it expired",
        InvitationState::Exhausted => "too many attempts failed",
        InvitationState::Cancelled => "it was cancelled",
        InvitationState::Declined => "the words were declined",
        InvitationState::Consumed | InvitationState::Open => "it was used",
    }
}

/// Unregisters a pairing's prompt route when its request ends, and with
/// `cancel`, cancels the invitation if it is still open: nobody is left to
/// answer for it.
struct PairingRoute {
    svc: Arc<Service>,
    invitation: InvitationId,
    cancel: bool,
}

impl Drop for PairingRoute {
    fn drop(&mut self) {
        lock(&self.svc.routes).pairing.remove(&self.invitation);
        if self.cancel {
            let _ = self.svc.with_node(|n| n.cancel_invitation(self.invitation));
        }
    }
}
