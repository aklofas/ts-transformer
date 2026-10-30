# Accept or refuse SRT callers by stream ID

> **When to use this:** One SRT listener port serves several publishers, and you want each stream ID admitted, given its own passphrase, or refused during the handshake.

> **Related:**
> - [Receive into a file](/docs/cookbook/receiving/receive-to-file.md)
> - [SRT guide](/docs/guides/srt.md)
> - [MPEG-TS demux guide](/docs/guides/mpegts-demux.md)

A caller names itself with the SRT stream ID. The listener keeps a rule table
keyed by that ID and answers inside the handshake, so a refused caller gets
its reason at once instead of connecting and being dropped later.

## Rust

```rust
use tst_srt::error::RejectReason;
use tst_srt::{ListenRule, Passphrase, SrtListenerBuilder};

let mut listener = SrtListenerBuilder::new().bind("0.0.0.0:9000")?;

// Take the handle before the accept loop: `accept()` borrows the listener
// mutably. The handle is a cheap clone of the same table.
let rules = listener.rules();

rules.set("publish:amber", ListenRule::AcceptWith {
    passphrase: Passphrase::new("amber-secret-1")?,
})?;
rules.set("publish:cobalt", ListenRule::Accept)?;
rules.set_default(ListenRule::Reject(RejectReason::NotFound))?; // allow-list

loop {
    let (socket, peer) = listener.accept()?;
    if let Some(id) = socket.stream_id() {
        // One live publisher per name from now on.
        rules.set(id, ListenRule::Reject(RejectReason::Conflict))?;
        // ... hand `socket` to a worker. When it ends, put the name's
        // original rule back (`rules.set(id, original)`); `remove` only a
        // name that had no rule, or the default rejects it next time.
    }
}
```

Stream IDs of the form `#!::u=alice,r=cam1` carry fields; parse them with
`AccessControl::parse(id)` and read `user()`, `resource()`, `mode()` or
`get("key")`. A free-form ID returns `AccessControlError::NotAccessControl`.

Runnable version: `examples/receiving/srt_listener_rules.rs`.

## Python

```python
from tstrans.srt import ListenRule, RejectReason

rules = listener.rules()
rules.set("publish:amber", ListenRule.accept_with("amber-secret-1"))
rules.set("publish:cobalt", ListenRule.accept())
rules.set_default(ListenRule.reject(RejectReason.NOT_FOUND))

# after accepting a publisher:
rules.set(stream_id, ListenRule.reject(RejectReason.CONFLICT))
# when it ends, restore the name's original rule; remove() only a name
# that had none (otherwise the default reject locks it out):
rules.set(stream_id, original_rule)
```

## Java

```java
import org.tstrans.srt.ListenRule;
import org.tstrans.srt.ListenRules;
import org.tstrans.srt.RejectReason;

ListenRules rules = listener.rules();
rules.set("publish:amber", ListenRule.acceptWith("amber-secret-1"));
rules.set("publish:cobalt", ListenRule.accept());
rules.setDefault(ListenRule.reject(RejectReason.NOT_FOUND));

// after accepting a publisher:
rules.set(streamId, ListenRule.reject(RejectReason.CONFLICT));
// when it ends, restore the name's original rule; remove() only a name
// that had none (otherwise the default reject locks it out):
rules.set(streamId, originalRule);
```

## Notes

- **Exact match.** A rule applies only when the caller's raw stream ID equals
  the rule's name byte for byte. There are no prefixes or patterns. A caller
  with no stream ID, an empty one, or one that is not UTF-8 gets the default
  rule.
- **Default is `Accept`.** With no rules, or no default set, an unlisted ID is
  accepted with the listener's own settings. Set a default `Reject` to turn the
  table into an allow-list.
- **Reject codes start at 1000.** Lower codes belong to libsrt, and `set`
  refuses them. The caller sees the refusal immediately as
  `SrtConnectError::Rejected` carrying your reason, for example 1404 for
  `NotFound`, 1409 for `Conflict`, 1403 for `Forbidden`, 1402 for `Overload`.
- **`AcceptWith` carries a passphrase only.** The key length follows the
  caller's key material. A caller with the wrong passphrase is refused by
  libsrt with `BadSecret`.
- **Edit from any thread.** Clone the handle before `accept()` parks and move
  it where you need it. An edit applies to the next handshake. After the
  listener closes, every edit returns a `Closed` error.
- **Race window.** A rule installed after `accept()` returned does not cover a
  caller whose handshake had already completed. Two same-name publishers that
  connect inside that window (typically well under a millisecond, longer
  under scheduling delay) are both accepted. Keep your own post-accept check
  as the backstop.
- **Peer-address decisions stay post-accept.** Rules see the stream ID only.
  To allow or refuse by source address, inspect the peer returned by
  `accept()` and close the socket yourself.
