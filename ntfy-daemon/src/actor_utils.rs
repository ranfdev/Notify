macro_rules! send_command {
    ($self:expr, $command:expr) => {{
        let (resp_tx, rx) = oneshot::channel();
        $self
            .command_tx
            .send($command(resp_tx))
            .await
            .map_err(|_| anyhow::anyhow!("Actor mailbox error"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("Actor response error"))?
    }};
}

pub(crate) use send_command;
