#!/usr/bin/env sh
# a simple script for testing migration of running process memory.
#
# this script will write a bunch of "hello"s to an in-memory string, then
# suspend to wait for a migration to occur. after the migration happens, the
# script can be foregrounded, and it will then check that the data is still
# there.
dirt="hello"
len=8192 # 8 KiB ought to be enough for anyone

# store 8k of data in memory
i=0
while [ "$i" -lt "$len" ]
do
    dirt="$dirt
hello"
    i=$(( i + 1 ))
done

echo "made dirt"

# suspend this process and wait for it to be resumed before checking that the
# data still exists.
#
# N.B.: posix sh doesn't have a suspend builtin, but we can make our own!
kill -s TSTP $$

# check that the data is still correct.
#
# we do this by writing it out to a file and then looping through the file,
# because i wasn't sure how to loop over a variable line by line in posix sh
# without using a file.
dirtfile="/tmp/dirt.txt"
echo "$dirt" > "$dirtfile"
actual_len=$(wc -l "$dirtfile" | cut -d " " -f 1)
echo "found $actual_len lines of dirt"

# pre-check the file's length
if [ "$actual_len" -lt "$len" ]
then
    echo "not enough dirt: $actual_len < $len"
    exit 1
fi

i=0
while read -r line
do
    if [ $i -eq 8192 ]; then
        echo "all good"
        exit 0
    fi
    if [ "$line" != "hello" ]
    then
        echo "bad dirt $i: $line"
        exit 1
    fi
    i=$(( i + 1 ))
done < "$dirtfile"