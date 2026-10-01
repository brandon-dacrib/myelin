# Factory half of this project's Sytest plugin: registers the implementation name `Myelin`
# (`run-tests.pl -I Myelin` selects it) and its one option, the path of the `hs` binary.
# SyTest::Homeserver::Myelin (../Homeserver/Myelin.pm) starts the server. Shape adapted from
# Sytest's lib/SyTest/HomeserverFactory/Dendrite.pm (Apache-2.0, Copyright 2017 New Vector Ltd);
# the values are this project's own. tests/sytest/README.md says how the plugin is found.

use strict;
use warnings;

require SyTest::Homeserver::Myelin;

package SyTest::HomeserverFactory::Myelin;
use base qw( SyTest::HomeserverFactory );

sub _init
{
   my $self = shift;

   $self->{args} = {
      binary => "/usr/local/bin/hs",
   };

   $self->SUPER::_init( @_ );
}

sub implementation_name
{
   return "myelin";
}

sub get_options
{
   my $self = shift;

   return (
      'myelin-binary=s' => \$self->{args}{binary},
      $self->SUPER::get_options(),
   );
}

sub print_usage
{
   print STDERR <<EOF
   --myelin-binary PATH       - path to the `hs` binary (default /usr/local/bin/hs)
EOF
}

sub create_server
{
   my $self = shift;
   my %params = ( @_, %{ $self->{args} } );

   return SyTest::Homeserver::Myelin->new( %params );
}

1;
