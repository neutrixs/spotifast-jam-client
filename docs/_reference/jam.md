---
title: Jams
description: Listening together through a jam server, what each listener's player does, and what a jam cannot do.
nav_order: 7
permalink: /jams/
---

A jam is a group listening to the same thing at the same time, each on
their own Spotify account and their own speakers. Spotify's own Jam has no
API another app can use, so Spotifast jams go through a small server of
your choosing, spotifast-jam-server, which only
passes messages between the listeners. Spotify Premium is needed, as for any
playback.

## Starting and joining

Jams live in **Connect to a device**, the speaker button in the player bar,
below your devices.

- **Start a jam** asks for the address of a jam server, such as
  `https://jam.example.com`, filled in with the last one you used. Starting
  makes you the host and copies the invite link.
- **Join** takes an invite link, `https://jam.example.com/jam/ABC234`. The
  link names its server, so there is nothing else to enter. Opening an
  invite link in a browser opens Spotifast and joins straight away, through
  the `spotifast-jam:` link the installers register.

While you are in a jam, the same section shows its code, who hosts it and
who is listening, with **Copy invite link** and **Leave jam** (**End jam**
for the host). **Jam · 3** beside the speaker button shows a jam is on and
opens the section.

While in a jam the queue panel shows the jam, the same for everyone in it:
**Now playing** is the host's song, **Queue** the jam's queue, and **Next
up** what the host's playlist or album plays after it. Anyone can play a
song in the jam's queue now (or double-click it), move it up or down, or
remove it, and **Clear** empties it. Double-clicking a Next up song plays
the host's playlist from there.

## What happens

- **The host's player decides what plays.** Whatever the host starts, and
  however it moves on, everyone else hears too.
- **Everyone else follows.** Their player starts the host's song at the
  host's place, from the same playlist or album when there is one, so the
  next song matches. Syncing is loose on purpose: a player more than five
  seconds out, for two seconds, seeks back in step, at most every fifteen
  seconds, and a moment of difference (a song loading, a seek landing) is
  never corrected. A host who pauses pauses everyone, within a second.
- **Everyone has the host's controls.** Play, pause, seeking, Next and
  Previous, shuffle and repeat, and starting a song, album or playlist all go
  to the host's player. Pause, seek, shuffle and repeat act at once on your
  own player too, and the jam waits for the host to catch up rather than
  undoing them. Everyone follows the host's shuffle and repeat.
- **One shared queue.** Add to queue and dropped songs go to the end of the
  jam's queue, Play next to its front, labelled with who added them. Anyone
  reorders it and removes any song. The host's player is handed only the
  jam's next song, through its own queue, so it plays before the playlist
  carries on; each leaves the list when it starts and the next is handed
  over. Clear queue clears the jam's queue too. An album added to the jam
  goes into the host's queue as its songs and leaves the jam's list. Local
  files are not shared.
- **Leaving.** Leaving drops you out. The host leaving, or being unreachable
  for five minutes, ends the jam for everyone.

## What it cannot do

- A song already handed to the host's queue, then moved down or removed,
  is taken back only while the host plays on this computer. On another
  device (Spotify's queue cannot be rewritten there) it still plays next,
  and the jam carries on after it.
- Volume stays each listener's own.
- A song one listener's account cannot play (region, explicit filter) stays
  unplayed for them while the others carry on.
- Each player starts songs in its own time, so listeners can be up to a
  second or so apart. It suits listening together, not a shared speaker.

See [Privacy](/privacy/) for what the jam server receives.
