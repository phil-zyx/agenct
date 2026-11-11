'use strict';
// Tests ONLY the acceptance runner's network-fault mechanism, not Agenct itself.
const assert = require('node:assert/strict');
const http = require('node:http');
const { test } = require('node:test');
const { Fixture } = require('./three-person-acceptance.cjs');

test('HTTP fault harness distinguishes an unsent write from a lost committed response', async () => {
  let writes = 0;
  const target = http.createServer((request, response) => {
    request.resume();
    request.once('end', () => {
      writes++;
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end('{}');
    });
  });
  await new Promise(resolve => target.listen(0, '127.0.0.1', resolve));
  const fixture = new Fixture('/unused-test-binary', target.address().port);
  try {
    await fixture.loseResponse('alice', '/synthetic-test', { body: 'synthetic' }, false);
    assert.equal(writes, 0, 'Before-forward cut must not dispatch a write');
    await fixture.loseResponse('alice', '/synthetic-test', { body: 'synthetic' }, true);
    assert.equal(writes, 1, 'After-commit cut must observe one successful upstream response');
  } finally {
    await fixture.dispose();
    target.closeAllConnections();
    await new Promise(resolve => target.close(resolve));
  }
});

test('a failed upstream operation cannot be reported as a committed-response cut', async () => {
  const target = http.createServer((request, response) => {
    request.resume();
    request.once('end', () => { response.writeHead(409); response.end('{}'); });
  });
  await new Promise(resolve => target.listen(0, '127.0.0.1', resolve));
  const fixture = new Fixture('/unused-test-binary', target.address().port);
  try {
    await assert.rejects(() => fixture.loseResponse('alice', '/synthetic-test', {}, true),
      /Cut must happen after a successful Hub response/);
  } finally {
    await fixture.dispose();
    target.closeAllConnections();
    await new Promise(resolve => target.close(resolve));
  }
});
