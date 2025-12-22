#!/bin/bash

for x in `pidof pingpong`; do echo "making $x RT priority process"; sudo chrt -f -p 99 $x; done

