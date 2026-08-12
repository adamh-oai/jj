---
title: "Concurrency and lifetime rules"
description: "SQLite writers, snapshot ownership, lease renewal, descriptor lifetime, and cross-client isolation."
sidebar:
  order: 4
---
The intended ownership rules are:

- Cut sequence reservation and publication are writer-serialized per watch.
- Long Btrfs ioctls, snapshot cuts, index walks, and projection should run
  outside SQLite writer transactions.
- Concurrent requests for the same eligible cut should join the existing work.
- Query leases retain exactly the required immutable snapshots/revisions until
  a response is written or a direct scan transaction finishes.
- Direct Begin, Renew, and Finish must not hold one global mutex across
  expensive unrelated cuts or potentially blocking writes.
- Lease expiration and advertised client deadlines must use one coherent
  monotonic time base and begin only after the lease can actually be returned.
- Abandoned responses and sessions require bounded deadlines and a maintenance
  path even if no subsequent client request arrives.
- Connection workers, queued frames, packet buffers, active sessions, and
  completed-session tombstones require bounded resource policies.

The current daemon has a per-connection OS-thread model. Its Watchman path has
some split begin/execute/finish machinery for concurrent cuts; the direct scan
path serializes every operation behind one handler mutex and also holds the
shared facade mutex during `Service::changes`. Neither direct socket operation
currently has a read/write deadline.
