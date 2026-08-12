---
title: "Previously resolved findings"
description: "Earlier review observations that no longer describe the current implementation."
sidebar:
  order: 8
---
- Exact historical baseline resolution now requires the same cut sequence and
  target snapshot UUID; it no longer accepts a merely older retained boundary.
- Automatic daemon startup creates its spool directory recursively with private
  permissions; the earlier missing-spool finding is obsolete.
- Direct scan handlers now resolve and activate each requested canonical root
  on demand; they are no longer bound to only the daemon's first root.
