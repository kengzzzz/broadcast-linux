#!/usr/bin/env python3
"""Keeps every CPU core busy for the given number of seconds, as load for audio tests."""
import multiprocessing
import sys
import time


def spin(end):
    while time.time() < end:
        pass


if __name__ == "__main__":
    end = time.time() + float(sys.argv[1])
    workers = [multiprocessing.Process(target=spin, args=(end,)) for _ in range(multiprocessing.cpu_count())]
    for w in workers:
        w.start()
    for w in workers:
        w.join()
