#!/bin/sh
# Pass when hello.txt exists and contains exactly 'hello world'.
[ -f hello.txt ] && [ "$(cat hello.txt)" = "hello world" ]
