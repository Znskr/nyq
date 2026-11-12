#!/bin/sh
grep -q '^DONE$' notes.md && ! grep -q 'TODO' notes.md
