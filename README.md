# Sonos til Homewright

En ekstern integration til [Homewright](https://github.com/magnusdunker/homewright), der styrer Sonos-højttalere som medieafspillere. Den taler UPnP med afspillerne på det lokale netværk (port 1400), uden om Sonos' cloud.

## Installation

I Homewright: **Indstillinger → Integrationer → Fra GitHub**. Indsæt

```
https://github.com/magnusdunker/homewright-sonos
```

og tryk **Installér**. Tilføj derefter **Sonos** under **Tilføj integration**. Formularen viser de afspillere, der blev fundet på netværket. Står alle valgt, kommer afspillere, der dukker op senere, også med. Når integrationen er installeret, foreslår Homewright den også selv, når den finder en Sonos-afspiller.

Der er kun én opsætning, og den dækker hele husstanden. Valget af afspillere kan ændres under integrationens indstillinger.

### Afspillere, som søgningen ikke finder

Afspillerne findes med mDNS (`_sonos._tcp`) og SSDP, som kun virker på samme netværk. Én fundet afspiller er nok: den kender alle de andre i husstanden. Står afspillerne på et andet VLAN, eller kører Homewright i Docker uden host-netværk, så skriv en afspillers adresse under **Ekstra adresser**, f.eks. `192.168.20.15`. Flere adresser adskilles med komma. Adressen prøves, når du gemmer, og integrationen prøver igen hvert 30. sekund, hvis afspilleren er væk ved opstart.

## Hvad du får

Hver synlig afspiller bliver en enhed i Homewright med én medieafspiller. Navnet er rummets navn fra Sonos-appen. Et stereopar, en Sub og surround-højttalere hører til den afspiller, de er koblet til, og får ingen entity for sig.

Tilstanden er **playing**, **paused**, **buffering** eller **idle**. En afspiller i en gruppe viser det, gruppen spiller, og har sin egen lydstyrke.

Attributterne følger Home Assistant: `volume_level`, `is_volume_muted`, `media_title`, `media_artist`, `media_album_name`, `media_channel` (radiostationen), `media_content_id`, `media_content_type`, `media_duration`, `media_position`, `media_position_updated_at`, `entity_picture`, `shuffle`, `repeat`, `source`, `source_list`, `group_members` og `supported_features`.

Tjenesterne er:

- `media_play`, `media_pause`, `media_play_pause`, `media_stop`, `media_seek`, `media_next_track` og `media_previous_track`. De sendes til gruppens koordinator, så de virker fra enhver afspiller i gruppen.
- `volume_set`, `volume_mute`, `volume_up` og `volume_down` (5 procentpoint ad gangen) for afspilleren selv
- `shuffle_set` og `repeat_set` (`off`, `all` eller `one`)
- `select_source`: en Sonos-favorit ved navn, eller `Line-in` og `TV` på afspillere, der har dem. En radiofavorit spilles direkte; en playliste eller et album erstatter køen.
- `clear_playlist` tømmer køen
- `join` sætter afspillerne i `group_members` i gruppe med denne, og `unjoin` tager afspilleren ud af sin gruppe:

```yaml
action: media_player.join
target:
  entity_id: media_player.stue
data:
  group_members:
    - media_player.kokken
    - media_player.bad
```

- `play_media` afspiller en URL eller en favorit:

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue
data:
  media_content_id: http://nas.lan/musik/sang.mp3
  media_content_type: music
  enqueue: replace        # eller add og next, der lægger den i køen
  extra:
    title: Sang
```

```yaml
action: media_player.play_media
target:
  entity_id: media_player.stue
data:
  media_content_id: FV:2/3         # favorittens id, se diagnosticeringen
  media_content_type: favorite_item_id
```

Favoritterne læses ved start og derefter hvert 5. minut.

### Hændelser og polling

Integrationen abonnerer på hver afspillers UPnP-hændelser (GENA) og lytter efter dem på en tilfældig port. Når en afspiller melder en ændring, læses dens tilstand med det samme, og ellers kun hvert minut. Ændres en gruppe, opdateres alle dens afspillere.

Kommer der ingen hændelser, fordi afspillerne ikke kan nå Homewright (Docker uden host-netværk, en firewall eller et andet VLAN), læser integrationen i stedet hver afspiller hvert 5. sekund. Det sker af sig selv. Afspilleren bliver utilgængelig, når den ikke svarer, og kommer tilbage, når den svarer igen.

Integrationen husker de adresser, søgningen sidst gav, og bruger dem ved opstart. Den søger igen hvert 5. minut og læser husstandens grupper hvert minut, så en afspiller, der skifter adresse eller kommer til, bliver fundet.

### Det kan den ikke endnu

Alarmer, oplæsning og lyde oven i musikken, natlyd, taleforstærkning, equalizer og gennemsyn af musiktjenester er ikke med.

## Udvikling

Bygget i Rust på SDK'en `thehome-plugin` fra [homewright-example-integration-rust](https://github.com/magnusdunker/homewright-example-integration-rust).

- `src/soap.rs`: SOAP-kald, hændelsesabonnementer og afspillerens beskrivelse
- `src/xml.rs`: SOAP-svar, DIDL-Lite og husstandens grupper (`ZoneGroupState`)
- `src/state.rs`: fra afspillerens svar til tilstand og attributter
- `src/player.rs`: én afspiller, dens abonnementer og tjenesterne
- `src/events.rs` og `src/http.rs`: modtageren af hændelser
- `src/discovery.rs`: mDNS og SSDP
- `src/lib.rs`: opsætningen og forbindelsen til Homewright
- `tests/`: tests mod falske afspillere i `tests/fake` og et falsk Homewright i `tests/host`

```
cargo test
cargo build --release
```

En ny version udgives med et tag, f.eks. `git tag v0.2.0 && git push origin v0.2.0`. Workflowet bygger til Windows, Linux (x86_64 og arm64, statisk musl) og macOS og lægger binærerne på releasen.

## Licens

MIT
