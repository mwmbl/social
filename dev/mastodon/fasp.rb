# Do what a Mastodon admin does in Admin > FASP, for end-to-end tests.
# Run with: docker compose run --rm web bin/rails runner /seed/fasp.rb <action> [capability]
#   approve            confirm the newest provider (Mastodon then calls our /provider_info)
#   enable <id>        enable a capability (Mastodon then POSTs our activation endpoint)
#   disable <id>       disable it (DELETE)
#   show               print providers and their capabilities
action, capability = ARGV
provider = Fasp::Provider.order(created_at: :desc).first
abort 'no FASP provider has registered' unless provider

case action
when 'approve'
  provider.update_info!(confirm: true)
when 'enable', 'disable'
  attrs = provider.capabilities.each_with_index.to_h do |c, i|
    enabled = c.id == capability ? action == 'enable' : c.enabled
    [i.to_s, { 'id' => c.id, 'version' => c.version, 'enabled' => enabled }]
  end
  provider.update!(capabilities_attributes: attrs)
end

Fasp::Provider.find_each do |p|
  puts "#{p.id} #{p.name} #{p.base_url} confirmed=#{p.confirmed} " \
       "capabilities=#{p.capabilities.map { |c| "#{c.id}@#{c.version}#{c.enabled ? '+' : '-'}" }.join(',')}"
end
