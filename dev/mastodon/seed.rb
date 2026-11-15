# Test data for end-to-end checks. Run with: docker compose run --rm web bin/rails runner /seed/seed.rb
# alice: discoverable and indexable, with a public and an unlisted post.
# bob: discoverable but not indexable, with a public post.
def user(name, indexable:)
  account = Account.find_local(name)
  account ||= begin
    User.create!(email: "#{name}@mastodon.test", password: SecureRandom.hex(16), agreement: true,
                 confirmed_at: Time.now.utc, approved: true, account_attributes: { username: name }).account
  end
  account.user.update!(approved: true) unless account.user.approved?
  account.update!(discoverable: true, indexable: indexable, display_name: name.capitalize,
                  note: "#{name.capitalize} tests Mwmbl Social")
  account
end

alice = user('alice', indexable: true)
bob = user('bob', indexable: false)

def post(account, text, visibility)
  status = PostStatusService.new.call(account, text: text, visibility: visibility)
  puts "#{account.username} #{visibility}: #{ActivityPub::TagManager.instance.uri_for(status)}"
end

post(alice, "Planting garlic in the allotment today #gardening #{SecureRandom.hex(3)}", :public)
post(alice, "An unlisted thought about compost #{SecureRandom.hex(3)}", :unlisted)
post(bob, "Bob would rather not be searched: turnips #{SecureRandom.hex(3)}", :public)
