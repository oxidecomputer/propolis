#!/usr/bin/env sh
set -m

DIRT="hello"
i=0
while [ $i -lt 8192 ]
do
    DIRT="$DIRT
hello"
    i=$(( i + 1 ))
done

echo "made dirt"

kill -s TSTP $$

echo "$DIRT" > dirt.txt
i=0
while read -r line
do
    if [ $i -eq 8192 ]
    then
        echo "all good"
        exit 0
    fi
    if [ "$line" != "hello" ]
    then
        echo "bad dirt $i: $line"
        exit 1
    fi
    i=$(( i + 1 ))
done < dirt.txt