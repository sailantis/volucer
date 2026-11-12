<?php

declare(strict_types=1);

require __DIR__ . '/vendor/autoload.php';

use Volucer\Request;
use Volucer\Response;
use Volucer\Supervisor;

// The same application as `worker.php`, served through the `attach` backend instead.
//
//     php supervisor.php                 # the relay from VOLUCER_RELAY_SOCKET
//     php supervisor.php unix:/run/app/workers.sock
//
// `Supervisor::serve()` forks a child per connection from the heap it inherited, so an
// application that boots a container or warms a router does it once instead of once per worker.
// That is the whole reason the backend exists: an `exec`ed worker cannot share an OPcache
// segment, and a forked one can.
//
// **Unix only.** It needs `ext-pcntl`, which does not exist on Windows; both example images
// include it. Use `kind = "worker"` there instead.
Supervisor::serve(
    Supervisor::relay($argv[1] ?? ''),
    static function (Request $request): Response {
        $path = (string) parse_url($request->uri, PHP_URL_PATH);

        if ($path === '/health') {
            return Response::json(['status' => 'ok', 'pid' => getmypid(), 'php' => PHP_VERSION]);
        }

        return Response::text("hello from attach pid " . getmypid() . " on PHP " . PHP_VERSION . "\n");
    },
);
