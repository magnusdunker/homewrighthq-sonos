# Sonos for HomewrightHQ

An external integration for [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) that controls Sonos speakers as media players. It talks UPnP with the players on the local network (port 1400), bypassing the Sonos cloud.

## Installation

In HomewrightHQ: **Settings → Integrations → From GitHub**. Paste

```
https://github.com/magnusdunker/homewrighthq-sonos
```

and press **Install**. Then add **Sonos** under **Add integration**. The form shows the players that were found on the network. If all are selected, players that show up later are included too. Once the integration is installed, HomewrightHQ also suggests it by itself when it finds a Sonos player.

There is only one entry, and it covers the whole household. The choice of players can be changed under the integration's options.

### Players that discovery does not find

The players are found with mDNS (`_sonos._tcp`) and SSDP, which only work on the same network. One found player is enough: it knows all the others in the household. If the players are on another VLAN, or HomewrightHQ runs in Docker without host networking, enter a player's address under **Ekstra adresser** (extra addresses), e.g. `192.168.20.15`. Separate several addresses with commas. The address is tried when you save, and the integration tries again every 30 seconds if the player is gone at startup.

## What you get

Each visible player becomes a device in HomewrightHQ with one media player. The name is the room name from the Sonos app. A stereo pair, a Sub and surround speakers belong to the player they are bonded to and get no entity of their own.

The state is **playing**, **paused**, **buffering** or **idle**. A player in a group shows what the group is playing, and has its own volume.

The attributes follow Home Assistant: `volume_level`, `is_volume_muted`, `media_title`, `media_artist`, `media_album_name`, `media_channel` (the radio station), `media_content_id`, `media_content_type`, `media_duration`, `media_position`, `media_position_updated_at`, `entity_picture`, `shuffle`, `repeat`, `source`, `source_list`, `group_members` and `supported_features`.

The services are:

- `media_play`, `media_pause`, `media_play_pause`, `media_stop`, `media_seek`, `media_next_track` and `media_previous_track`. They are sent to the group's coordinator, so they work from any player in the group.
- `volume_set`, `volume_mute`, `volume_up` and `volume_down` (5 percentage points at a time) for the player itself
- `shuffle_set` and `repeat_set` (`off`, `all` or `one`)
- `select_source`: a Sonos favorite by name, or `Line-in` and `TV` on players that have them. A radio favorite is played directly; a playlist or an album replaces the queue.
- `clear_playlist` empties the queue
- `join` groups the players in `group_members` with this one, and `unjoin` takes the player out of its group:

```yaml
action: media_player.join
target:
  entity_id: media_player.stue
data:
  group_members:
    - media_player.kokken
    - media_player.bad
```

- `play_media` plays a URL or a favorite:

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue
data:
  media_content_id: http://nas.lan/musik/sang.mp3
  media_content_type: music
  enqueue: replace        # or add and next, which put it in the queue
  extra:
    title: Sang
```

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue
data:
  media_content_id: FV:2/3         # the favorite's id, see the diagnostics
  media_content_type: favorite_item_id
```

The favorites are read at startup and then every 5 minutes.

### Events and polling

The integration subscribes to each player's UPnP events (GENA) and listens for them on a random port. When a player reports a change, its state is read straight away, and otherwise only every minute. If a group changes, all its players are updated.

If no events arrive, because the players cannot reach HomewrightHQ (Docker without host networking, a firewall or another VLAN), the integration reads each player every 5 seconds instead. This happens by itself. The player becomes unavailable when it does not respond, and comes back when it responds again.

The integration remembers the addresses discovery last gave and uses them at startup. It searches again every 5 minutes and reads the household's groups every minute, so a player that changes address or is added gets found.

### What it cannot do yet

Alarms, announcements and sounds on top of the music, night sound, speech enhancement, equalizer and browsing music services are not included.

## Development

Built in Rust on the `thehome-plugin` SDK from [homewright-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/soap.rs`: SOAP calls, event subscriptions and the player's description
- `src/xml.rs`: SOAP responses, DIDL-Lite and the household's groups (`ZoneGroupState`)
- `src/state.rs`: from the player's responses to state and attributes
- `src/player.rs`: one player, its subscriptions and the services
- `src/events.rs` and `src/http.rs`: the event receiver
- `src/discovery.rs`: mDNS and SSDP
- `src/lib.rs`: the setup and the connection to HomewrightHQ
- `tests/`: tests against fake players in `tests/fake` and a fake HomewrightHQ in `tests/host`

```
cargo test
cargo build --release
```

A new version is released with a tag, e.g. `git tag v0.2.0 && git push origin v0.2.0`. The workflow builds for Windows, Linux (x86_64 and arm64, static musl) and macOS and puts the binaries on the release.

## License

MIT
