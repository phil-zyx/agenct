#!/usr/bin/env python3
"""Generate ephemeral local-development bearer tokens. Never commit the output."""
import hashlib,json,secrets
users=[]
for user in ('alice','bob'):
    token=secrets.token_urlsafe(32)
    print(f'{user} token (paste only into local UI): {token}')
    users.append({'id':user,'token_sha256':hashlib.sha256(token.encode()).hexdigest()})
print("\nexport AGENCT_USERS_JSON='"+json.dumps(users)+"'")
print('export AGENCT_RUNTIME=mock')
print('export AGENCT_DATABASE=/tmp/agenct-dev.sqlite3')
