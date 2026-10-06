#!/usr/bin/env ruby
# A hung daemon request must fail the scenario with evidence, before the CI job kills it.
require 'timeout'

seconds = Float(ARGV.shift)
daemon_pid = Integer(ARGV.shift)
abort 'usage: bounded_cli.rb SECONDS DAEMON_PID COMMAND [ARGS...]' if seconds <= 0 || ARGV.empty?

pid = Process.spawn(*ARGV)
begin
  _, status = Timeout.timeout(seconds) { Process.waitpid2(pid) }
  exit(status.exitstatus || 128 + status.termsig)
rescue Timeout::Error
  warn "FAIL: local CLI request exceeded #{seconds}s (CLI #{pid}, daemon #{daemon_pid})"
  if daemon_pid.positive?
    system('ps', '-p', "#{pid},#{daemon_pid}", '-o', 'pid,ppid,stat,etime,command', out: $stderr)
    if RUBY_PLATFORM.include?('darwin')
      # sample has its own one-second duration; fixture processes contain no user credentials.
      system('/usr/bin/sample', daemon_pid.to_s, '1', out: $stderr, err: $stderr)
    end
  end
  Process.kill('KILL', pid) rescue nil
  Process.waitpid(pid) rescue nil
  exit 124
end
