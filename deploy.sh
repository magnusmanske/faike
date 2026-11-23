#!/usr/bin/env bash
# Delopy and restart toolforge services
ssh login.toolforge.org 'become faike bash -lc "cd ~/faike && git pull && ./build.sh && ./restart.sh && ./webservice.sh && echo \"OK\""'
