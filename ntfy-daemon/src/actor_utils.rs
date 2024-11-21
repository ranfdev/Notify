macro_rules! send_command {
    ($self:expr, $command:expr) => {{
        let (resp_tx, rx) = oneshot::channel();
        use anyhow::Context;
        $self
            .command_tx
            .send($command(resp_tx))
            .await
            .context("Actor mailbox error")?;
        rx.await.context("Actor response error")?
    }};
}

pub(crate) use send_command;
