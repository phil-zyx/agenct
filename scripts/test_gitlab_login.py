import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('setup_gitlab',Path(__file__).with_name('configure-gitlab-login.py'))
setup=importlib.util.module_from_spec(spec);spec.loader.exec_module(setup)
class GitLabConfiguration(unittest.TestCase):
    def test_reuses_credentials_without_touching_source_or_existing_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); source=root/'source.env'; mapping=root/'subjects.json'; state=root/'new'
            original='GITLAB_OAUTH_BASE_URL=https://git.qivators.com\nGITLAB_OAUTH_CLIENT_ID=test-client\nGITLAB_OAUTH_CLIENT_SECRET=never-print-this\n'
            source.write_text(original);mapping.write_text('{"7":"alice","8":"bob"}')
            args=['setup','--source-env',str(source),'--subjects',str(mapping),'--state-dir',str(state),'--admin','alice']
            with patch.object(sys,'argv',args),patch('builtins.print') as output: setup.main()
            self.assertNotIn('never-print-this',str(output.call_args_list))
            self.assertEqual(source.read_text(),original)
            config=json.loads((state/'browser-oidc.json').read_text())
            self.assertTrue(config['gitlab_oauth']);self.assertEqual(config['subjects'],{'7':'alice','8':'bob'})
            self.assertEqual(config['token_endpoint'],'https://git.qivators.com/oauth/token')
            self.assertEqual((state/'client-secret').stat().st_mode & 0o777,0o600)
            self.assertFalse((state/'hub.sqlite3').exists())
            with patch.object(sys,'argv',args),self.assertRaisesRegex(ValueError,'already exists'): setup.main()
    def test_auto_signup_has_no_legacy_mapping_or_inferred_admin(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';source.write_text('GITLAB_OAUTH_CLIENT_ID=id\nGITLAB_OAUTH_CLIENT_SECRET=secret')
            state=root/'new';args=['setup','--source-env',str(source),'--auto-signup','--state-dir',str(state)]
            with patch.object(sys,'argv',args),patch('builtins.print'): setup.main()
            config=json.loads((state/'browser-oidc.json').read_text());launch=json.loads((state/'launch.json').read_text())
            self.assertTrue(config['gitlab_auto_signup']);self.assertEqual(config['subjects'],{})
            self.assertNotIn('AGENCT_MEMBER_ADMINS_JSON',launch);self.assertNotIn('AGENCT_TENANTS_FILE',launch)
            self.assertEqual(json.loads(launch['AGENCT_USERS_JSON'])[0]['id'],'login-bootstrap')
    def test_shared_login_defaults_to_original_http_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source'
            source.write_text('GITLAB_OAUTH_CLIENT_ID=id\nGITLAB_OAUTH_CLIENT_SECRET=secret\nGITLAB_OAUTH_REDIRECT_URI=http://localhost:5176/identity/v1/auth/oidc/gitlab/callback')
            state=root/'new'
            args=['setup','--source-env',str(source),'--auto-signup','--state-dir',str(state),'--shared-login-url','http://localhost:18082/identity/v1/auth/oidc/gitlab/login']
            with patch.object(sys,'argv',args),patch('builtins.print'): setup.main()
            config=json.loads((state/'browser-oidc.json').read_text());launch=json.loads((state/'launch.json').read_text())
            self.assertEqual(config['public_origin'],'http://localhost:8081')
            self.assertTrue(config['allow_loopback_http'])
            self.assertIn(':5176/',config['shared_gitlab_callback'])
            self.assertEqual(launch['AGENCT_BIND'],'127.0.0.1:8081')
            self.assertFalse((state/'Caddyfile').exists())
    def test_reuse_registered_callback_runs_app_on_5176(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source'
            source.write_text('GITLAB_OAUTH_CLIENT_ID=id\nGITLAB_OAUTH_CLIENT_SECRET=secret\nGITLAB_OAUTH_REDIRECT_URI=http://100.64.0.12:5176/identity/v1/auth/oidc/gitlab/callback')
            state=root/'new'
            args=['setup','--source-env',str(source),'--auto-signup','--state-dir',str(state),'--reuse-registered-callback']
            with patch.object(sys,'argv',args),patch('builtins.print'): setup.main()
            config=json.loads((state/'browser-oidc.json').read_text());launch=json.loads((state/'launch.json').read_text())
            self.assertEqual(config['public_origin'],'http://100.64.0.12:5176')
            self.assertEqual(config['callback_path'],'/identity/v1/auth/oidc/gitlab/callback')
            self.assertTrue(config['allow_private_http'])
            self.assertNotIn('shared_gitlab_login',config)
            self.assertEqual(launch['AGENCT_BIND'],'100.64.0.12:5176')
            self.assertFalse((state/'Caddyfile').exists())
    def test_rejects_duplicate_or_email_binding(self):
        for mapping in ['{"a@example.com":"alice"}','{"7":"alice","7":"bob"}','{"7":"alice","8":"alice"}']:
            with tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source=root/'source';source.write_text('GITLAB_OAUTH_CLIENT_ID=id\nGITLAB_OAUTH_CLIENT_SECRET=secret')
                subjects=root/'subjects';subjects.write_text(mapping)
                args=['setup','--source-env',str(source),'--subjects',str(subjects),'--state-dir',str(root/'new'),'--admin','alice']
                with patch.object(sys,'argv',args),self.assertRaises(ValueError): setup.main()
if __name__=='__main__': unittest.main()
