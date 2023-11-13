@0x9663f4dd604afa35;

enum Status {
    down @0;
    degraded @1;
    up @2;
}

interface WatchHandle {}

interface OutputChannel {
    sendMessage @0 (message: Text);
    sendStatus @1 (status: Status);
    done @2 ();
}

struct SubscriptionInfo {
    server @0 :Text;
    topic @1 :Text;
    displayName @2 :Text;
    muted @3 :Bool;
    readUntil @4 :UInt64;
}

interface Subscription {
    watch @0 (watcher: OutputChannel, since: UInt64) -> (handle: WatchHandle);
    publish @1 (message: Text);

    getInfo @2 () -> SubscriptionInfo;
    updateInfo @3 (value: SubscriptionInfo);
    updateReadUntil @4 (value: UInt64);

    clearNotifications @5 ();
}

struct Account {
    server @0 :Text;
    username @1 :Text;
}

interface SystemNotifier {
    subscribe @0 (server: Text, topic: Text) -> (subscription: Subscription);
    unsubscribe @1 (server: Text, topic: Text);
    listSubscriptions @2 () -> (list: List(Subscription));
    addAccount @3 (account: Account, password: Text);
    removeAccount @4 (account: Account);
    listAccounts @5 () -> (list: List(Account));
}
