# Pair your own machines

Pair your machines once, and each one gets full control of the others. It
can start, reopen, read and type into every session on the other machine,
manage sharing and settings, and send messages there. That covers sessions
started later too, so a new session, share or handoff needs no further
approval.

Pairing is the only step that grants anything. Signing in to the same
account does not: the server lists your account's machines, but each
machine trusts another only once you pair it.

## Prerequisites

- **Every machine runs a host** signed in to the same account and server:

  ```sh
  export CONVERSATIONS_SERVER=https://conversations.example.com
  conversations login
  conversations host
  ```

- **Each host presents its machine card** when it connects. A card is
  signed by the machine's identity key and names the key that machine signs
  operations with, plus the machine's name. Hosts from before this release
  have no card and are not listed.

## Pair from your phone (one tap)

This works when your browser already has **full control on every machine**
(every permission ticked when you approved it; see
[Give a browser access to your host](trust-onboarding.md)).

1. In the hosted app, open **Settings** → **Host access**.
2. Under **Pair my machines**, choose **Pair my machines**.

The browser signs one `machine.pair` for each machine, naming each of the
others. The identity keys come from the browser's own pairings, never from
the server. Each machine checks that the browser's key has full control
there and that the other machine signed its card, and only then pairs it.
A browser with less than full control is refused, because it can't give
away more than it holds.

## Pair at the machines (one click on each)

Use this when no device has full control on both machines yet.

1. On machine A, open the local host page (`conversations open`) →
   **Settings** → **Host access** → **Your machines**. It shows this
   machine's fingerprint and your account's other online machines.
2. Find machine B and compare its fingerprint with the one B's own page
   shows for itself. Tick the box and choose **Pair**.
3. Do the same on machine B for machine A.

You can do the same from a terminal:

```sh
conversations machine list            # this machine's fingerprint, and the others
conversations machine pair <fingerprint>
```

A fingerprint is the first 20 characters of the machine's host id, in groups
of four.

### A new machine meets all your machines

Pair a new machine with **one** of your machines and it is paired with all
of them. The machine you paired it on uses the full control it has on your
other machines to pass the new card on to each of them, and their cards on
to the new machine. If you pair from the new machine second, it introduces
itself and the other machine passes the pairing on then. A machine that is
offline is reported; pair it again once it is online.

## Messages between your machines

An agent can message a session on any paired machine without anyone pinning
that session. When the session isn't known locally, the server says which
machine runs it, and the message goes there only if that machine is paired.
If the server is wrong, the message reaches another of your paired machines,
which refuses a session it doesn't have.

## Unpair

On the local page choose **Unpair**, or run:

```sh
conversations machine unpair <fingerprint>
```

Unpairing revokes the other machine's access here at once, including work
already queued, and interrupts what it was doing (sessions keep running).
It then asks the other machine to drop this one too. If that machine is
offline, unpair this machine there as well.

Pairing again later works. It gives fresh grants, so nothing queued under
the earlier pairing comes back.

## What full control means

A paired machine, and any agent running on it, can do on the other machine
everything you can do there. A compromised machine can therefore drive
your others until you unpair it. Pair only machines you own.
