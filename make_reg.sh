#!/bin/bash

for x in `pidof pingpong`; do echo "making $x regular priority process"; sudo chrt -o -p 0 $x; done

