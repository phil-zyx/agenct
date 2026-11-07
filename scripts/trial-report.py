#!/usr/bin/env python3
"""Read-only daily trial counts, never conversation text or credentials. Dates are UTC."""
import argparse
from contextlib import closing
from datetime import date, datetime, timezone
import json
from pathlib import Path
import sqlite3


def report(database, day, missed_notifications=None, model_timeouts=None):
    date.fromisoformat(day)
    with closing(sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True)) as db:
        db.execute('BEGIN')  # One consistent read snapshot even while Hub writes.
        def count(sql, args=(day,)): return db.execute(sql, args).fetchone()[0]
        statuses = dict(db.execute('SELECT status,count(*) FROM proposals WHERE date(created_at)=? GROUP BY status', (day,)))
        proposals = sum(statuses.values())
        confirmed, rejected = statuses.get('confirmed', 0), statuses.get('rejected', 0)
        generated = count("SELECT count(*) FROM deliveries WHERE status='draft' AND date(created_at)=?")
        adopted = count("SELECT count(*) FROM deliveries d WHERE d.status='draft' AND date(d.created_at)=? AND EXISTS (SELECT 1 FROM deliveries s WHERE s.task_id=d.task_id AND s.status='submitted' AND (s.source_draft_id=d.id OR s.content=d.content))")
        durations = [r[0] for r in db.execute("SELECT CAST((julianday(a.at)-julianday(c.at))*86400 AS INTEGER) FROM events a JOIN events c ON c.task_id=a.task_id AND c.action='created' WHERE a.action='approve' AND date(a.at)=? AND a.at>=c.at", (day,))]
        return {'date_utc': day, 'delegations_created': count("SELECT count(*) FROM events WHERE action='created' AND date(at)=?"),
                'proposal_creation_cohort': {'total': proposals, 'confirmed_now': confirmed, 'rejected_now': rejected,
                    'confirmation_rate': confirmed/proposals if proposals else None, 'rejection_rate': rejected/proposals if proposals else None},
                'draft_creation_cohort': {'generated': generated, 'submitted_exact_or_linked_now': adopted,
                    'adoption_rate_lower_bound': adopted/generated if generated else None},
                'accepted_today': len(durations), 'delegation_to_acceptance_seconds': durations,
                'unfinished_tasks_now': count("SELECT count(*) FROM tasks WHERE status NOT IN ('completed','canceled')", ()),
                'failed_or_interrupted_messages': count("SELECT count(*) FROM messages WHERE status IN ('failed','interrupted') AND date(created_at)=?"),
                'uncertain_chat_turns_now': count("SELECT count(*) FROM chat_turns WHERE status='uncertain'", ()),
                'missed_notifications_reported': missed_notifications, 'model_timeouts_reported': model_timeouts,
                'notes': ['Proposal and draft rates are creation-cohort states at report time, not action timestamps.',
                          'Edited drafts without an explicit source link cannot be inferred; adoption is a lower bound.',
                          'Missed notifications and timeouts require operator observations; null means not measured.']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--date', default=datetime.now(timezone.utc).date().isoformat())
    parser.add_argument('--missed-notifications', type=int)
    parser.add_argument('--model-timeouts', type=int)
    args = parser.parse_args()
    if any(v is not None and v < 0 for v in (args.missed_notifications, args.model_timeouts)):
        parser.error('observation counts must be nonnegative')
    print(json.dumps(report(args.state_dir/'hub.sqlite3', args.date, args.missed_notifications, args.model_timeouts), ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
